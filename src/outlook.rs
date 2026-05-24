use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::error::Error;
use crate::provider_utils::{MAX_BLOB_BYTES, MAX_UPLOAD_CACHE_BYTES, UPLOAD_CACHE_CAP};
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
    /// Compose-time blob upload cache. Synthetic blob_ids returned to the
    /// frontend by `upload_blob` map to (mime, bytes) pairs here; resolved
    /// by `peek_blob_bytes` during send and dropped after a successful
    /// send via `drain_consumed_synthetic_blobs`. Same shape as Gmail's
    /// upload_cache — cap discipline shared via provider_utils constants.
    pub upload_cache: tokio::sync::Mutex<HashMap<uuid::Uuid, (String, Vec<u8>)>>,
    /// 60s TTL cache of the identity list (just `/me` today). Roborev
    /// 183 #3: avoids an extra Graph RTT on every send-with-from_addr,
    /// since `/me` fields are essentially session-immutable.
    pub identity_cache: tokio::sync::Mutex<Option<IdentityCacheEntry>>,
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

/// Identity cache TTL. /me data (mail / displayName / id) is essentially
/// session-immutable; the cache exists to avoid an extra RTT on every
/// send-with-from_addr (roborev 183 #3), not to track changing state.
pub const IDENTITY_CACHE_TTL: Duration = Duration::from_secs(60);

/// Snapshot of the identity list anchored at a fetch time for TTL math.
#[derive(Clone)]
pub struct IdentityCacheEntry {
    pub fetched_at: Instant,
    pub identities: Vec<crate::types::Identity>,
}

/// Cap on the page_cache to bound memory if a long-running session
/// accumulates many unique (folder, filter, search) queries. When the
/// cache reaches this size, the next insert evicts an arbitrary entry —
/// re-issuing the query just costs one Graph round-trip.
/// Roborev 179 #3.
pub const MAX_PAGE_CACHE_ENTRIES: usize = 32;

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

// Microsoft OAuth2 endpoints. We target `/consumers` (personal MSA tenant)
// rather than `/common`. Microsoft now blocks end-user consent to newly
// registered multitenant apps that have no verified publisher (MPN ID), so
// the multitenant audience is unusable for self-hosted single-user installs.
// The matching app registration must use "Personal Microsoft accounts only"
// (see README → Azure AD App Registration).
const AUTH_URL: &str = "https://login.microsoftonline.com/consumers/oauth2/v2.0/authorize";
const TOKEN_URL: &str = "https://login.microsoftonline.com/consumers/oauth2/v2.0/token";
const GRAPH_BASE: &str = "https://graph.microsoft.com/v1.0";
const REDIRECT_URI: &str = "http://localhost:8400/callback";

// User.Read is required for Graph /me to reliably return mail / userPrincipalName;
// without it, consent grants that carry only Mail/Calendar scopes can 403 on /me,
// which surfaces to the user as "Could not determine Outlook email address".
const SCOPES: &str = "User.Read Calendars.ReadWrite Mail.ReadWrite Mail.Send offline_access";

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
        upload_cache: tokio::sync::Mutex::new(HashMap::new()),
        identity_cache: tokio::sync::Mutex::new(None),
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
        upload_cache: tokio::sync::Mutex::new(HashMap::new()),
        identity_cache: tokio::sync::Mutex::new(None),
    };

    save_tokens(&session)?;
    tracing::info!("Outlook OAuth completed for {}", session.email);
    Ok(session)
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct MeResponse {
    #[serde(default)]
    mail: Option<String>,
    #[serde(default)]
    user_principal_name: Option<String>,
    #[serde(default)]
    other_mails: Vec<String>,
}

/// Fetch the authenticated user's email from Microsoft Graph.
///
/// Personal MSA accounts (outlook.com, hotmail.com, live.com) frequently return
/// `mail: null` and put the actual address in `otherMails[0]`, so we $select all
/// three candidate fields and fall through them in order. Without the HTTP status
/// check below, a 403 from Graph (e.g. missing User.Read consent) would parse
/// successfully as JSON and surface as the opaque "Could not determine Outlook
/// email address" — masking the real consent error from the user.
async fn fetch_user_email(client: &reqwest::Client, access_token: &str) -> Result<String, Error> {
    let url = format!("{GRAPH_BASE}/me?$select=mail,userPrincipalName,otherMails");
    let resp = client.get(&url).bearer_auth(access_token).send().await?;
    let status = resp.status();
    let text = resp
        .text()
        .await
        .map_err(|e| Error::Internal(format!("Reading Graph /me response body failed: {e}")))?;
    if !status.is_success() {
        return Err(classify_outlook_error("me.get", status, &text));
    }
    let me: MeResponse = serde_json::from_str(&text)
        .map_err(|e| Error::Internal(format!("Graph /me returned non-JSON: {e}: {text}")))?;
    me.mail
        .or(me.user_principal_name)
        .or_else(|| me.other_mails.into_iter().next())
        .ok_or_else(|| {
            Error::Internal(format!(
                "Could not determine Outlook email address from Graph /me response: {text}"
            ))
        })
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
    // Roborev 179 #8: use tokio::fs::remove_file so the async signature is
    // honest — the function is awaited from inside ensure_token's hot path
    // and a blocking remove_file would briefly stall the runtime.
    if tokio::fs::try_exists(&session.token_path)
        .await
        .unwrap_or(false)
        && let Err(e) = tokio::fs::remove_file(&session.token_path).await
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
    // Roborev 179 #6: multi-word subject values must be wrapped in KQL
    // quotes — otherwise `subject:meeting tomorrow` parses as
    // `subject:meeting AND tomorrow`.
    for sub in &q.subject {
        if sub.contains(char::is_whitespace) {
            search_parts.push(format!("subject:\"{}\"", escape_search_string(sub)));
        } else {
            search_parts.push(format!("subject:{sub}"));
        }
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
///
/// Roborev 179 #10 defense: msg_id and att_id must NOT contain `:` —
/// otherwise the formatted `outlook:msg:att` string would round-trip to
/// the wrong split when `BlobRef::parse` rebuilds it. Real Graph IDs are
/// long base64 (no colons), but we don't trust Graph blindly.
fn parse_graph_attachments(
    msg_id: &str,
    arr_json: &serde_json::Value,
) -> Vec<crate::types::Attachment> {
    if msg_id.contains(':') {
        tracing::warn!(
            msg_id = %msg_id,
            "Outlook message ID contains ':' — refusing to construct \
             BlobRefs (would round-trip incorrectly)"
        );
        return Vec::new();
    }
    arr_json
        .as_array()
        .map(|arr| {
            arr.iter()
                .filter_map(|att| {
                    let att_id = att["id"].as_str()?;
                    if att_id.contains(':') {
                        tracing::warn!(
                            att_id = %att_id,
                            "Outlook attachment ID contains ':' — skipping \
                             (would corrupt BlobRef round-trip)"
                        );
                        return None;
                    }
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

// =============================================================================
// Phase 4: Outlook email — mutation helpers (pure)
// =============================================================================

/// What `move_to_mailbox` should do for a given target folder ID. Mirrors
/// Gmail's `move_plan`. Pure — extracted for unit testing the "reject
/// system folders that need dedicated endpoints" discipline without HTTP.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum MovePlanOutlook {
    /// Reject with `BadRequest`. Static text — these are all fixed
    /// strings so the variant doesn't carry an owning String.
    Reject(&'static str),
    /// Issue `POST /me/messages/{id}/move` with this `destinationId`.
    Move { destination: String },
}

pub(crate) fn move_plan_outlook(target_folder_id: &str) -> MovePlanOutlook {
    match target_folder_id {
        // Drafts can only be reached via create / createReply, not /move.
        "drafts" => MovePlanOutlook::Reject(
            "Outlook: cannot move messages into Drafts. Drafts are created \
             via send/reply endpoints, not /me/messages/{id}/move.",
        ),
        // Sent gets the SENT flag set automatically when sendMail / send
        // is called — moving a non-sent message into it would be a lie.
        "sentitems" => MovePlanOutlook::Reject(
            "Outlook: cannot move messages into Sent. The Sent folder is \
             populated automatically when you send a message.",
        ),
        // "Mark as junk" needs the markAsJunk action so Outlook's junk
        // filter learns from the user signal; plain /move bypasses that.
        "junkemail" => MovePlanOutlook::Reject(
            "Outlook: 'Mark as junk' must use the markAsJunk action so the \
             junk filter learns — plain /move skips that signal.",
        ),
        // Everything else (inbox, archive, deleteditems, user folders) is
        // a valid /move target.
        other => MovePlanOutlook::Move {
            destination: other.to_string(),
        },
    }
}

/// JSON body for `PATCH /me/messages/{id}` to mark read / unread.
pub(crate) fn mark_read_body(is_read: bool) -> serde_json::Value {
    serde_json::json!({ "isRead": is_read })
}

/// JSON body for `PATCH /me/messages/{id}` to flag / unflag.
pub(crate) fn flag_body(flagged: bool) -> serde_json::Value {
    serde_json::json!({
        "flag": { "flagStatus": if flagged { "flagged" } else { "notFlagged" } }
    })
}

/// JSON body for `POST /me/messages/{id}/move`.
pub(crate) fn move_body(destination_folder_id: &str) -> serde_json::Value {
    serde_json::json!({ "destinationId": destination_folder_id })
}

/// Outcome of parsing a Graph `/$batch` response. Roborev 180 #4 + #5:
/// `succeeded` is the count of 2xx per-item responses; `errors` is the
/// aggregated list of per-item failures (caller can join into one
/// message rather than truncate to the first); `any_404` flags whether
/// any per-item failure was a 404 (used by archive_batch to surface the
/// "create Archive folder" actionable message).
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct BatchResult {
    pub succeeded: usize,
    pub errors: Vec<String>,
    pub any_404: bool,
}

/// Parse a Graph `/$batch` response JSON. Returns `Err(_)` for protocol
/// violations (missing `responses` array, count mismatch — roborev 180
/// #4), and `Ok(BatchResult)` otherwise (including partial failures —
/// the caller decides how to react). Pure; unit-tested.
pub(crate) fn parse_batch_response(
    body: &serde_json::Value,
    expected_count: usize,
) -> Result<BatchResult, String> {
    let arr = body["responses"].as_array().ok_or_else(|| {
        "Graph /$batch response missing `responses` array (malformed)".to_string()
    })?;
    if arr.len() != expected_count {
        return Err(format!(
            "Graph /$batch responses count mismatch: expected {expected_count}, got {}",
            arr.len()
        ));
    }
    let mut succeeded = 0usize;
    let mut errors: Vec<String> = Vec::new();
    let mut any_404 = false;
    for r in arr {
        let status = r["status"].as_u64().unwrap_or(0);
        if (200..300).contains(&status) {
            succeeded += 1;
        } else {
            if status == 404 {
                any_404 = true;
            }
            let item_id = r["id"].as_str().unwrap_or("?");
            let msg = r["body"]["error"]["message"]
                .as_str()
                .unwrap_or("(no error message)");
            errors.push(format!("item {item_id}: {status} {msg}"));
        }
    }
    Ok(BatchResult {
        succeeded,
        errors,
        any_404,
    })
}

/// Should this `Mailbox.role` reject `/move` calls? Roborev 180 #6:
/// drafts/sent/junk need dedicated endpoints (createReply, send,
/// markAsJunk respectively). Matches the literal-string branches in
/// `move_plan_outlook` so opaque folder IDs are caught equivalently.
pub(crate) fn role_rejects_move(role: Option<&str>) -> Option<&'static str> {
    match role {
        Some("drafts") => Some(
            "Outlook: cannot move messages into Drafts. Drafts are created \
             via send/reply endpoints, not /me/messages/{id}/move.",
        ),
        Some("sent") => Some(
            "Outlook: cannot move messages into Sent. The Sent folder is \
             populated automatically when you send a message.",
        ),
        Some("junk") => Some(
            "Outlook: 'Mark as junk' must use the markAsJunk action so the \
             junk filter learns — plain /move skips that signal.",
        ),
        _ => None,
    }
}

/// Friendly user-facing message for the "Archive folder missing" 404.
/// Shared between archive() (single-message 404) and archive_batch()
/// (per-item 404 in /$batch response). Roborev 180 #2.
pub(crate) fn archive_404_message() -> Error {
    Error::BadRequest(
        "This Outlook account has no Archive folder. Create one in \
         Outlook web (Settings → Mail → Layout → Archive folder), \
         then retry."
            .into(),
    )
}

/// Split message IDs into batches of at most 20 — Graph's `/$batch`
/// per-request cap. Empty input → empty output (no degenerate empty
/// chunks). Pure; tests assert boundary cases (1, exactly 20, 21, 50).
pub(crate) fn chunk_batch_requests(ids: &[String]) -> Vec<Vec<&str>> {
    /// Graph's documented `/$batch` per-request limit.
    const BATCH_SIZE: usize = 20;
    if ids.is_empty() {
        return Vec::new();
    }
    ids.chunks(BATCH_SIZE)
        .map(|chunk| chunk.iter().map(String::as_str).collect())
        .collect()
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
    // Roborev 179 #4: $top=100 silently truncates for users with many
    // folders. 999 is Graph's documented per-page max — covers practical
    // cases without following @odata.nextLink (which we'd need if users
    // legitimately exceed 999).
    let url = format!("{GRAPH_BASE}/me/mailFolders?$top=999");
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
            // Graph exposes wellKnownName for system folders; user folders
            // get `None` here. Roborev 179 #9: dropped the `id` fallback —
            // Graph returns opaque base64 IDs in `id`, never the well-known
            // strings, so the fallback could never fire.
            let role = f.well_known_name.as_deref().and_then(outlook_folder_role);
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

/// Invalidate the page cursor cache. Roborev 179 #3: mutations should
/// clear this alongside the folder cache — otherwise a cached
/// `@odata.nextLink` happily skips deleted/moved messages on subsequent
/// scrolls. Milestone B wires this into mark_read/archive/trash/move.
pub async fn invalidate_page_cache(session: &OutlookSession) {
    let mut cache = session.page_cache.lock().await;
    cache.clear();
}

/// Fetch user identities. Outlook is simpler than Gmail's sendAs — Graph
/// `/me` returns the single primary identity. Aliases via
/// `/me/mailboxSettings/aliases` exist on enterprise accounts but most
/// users don't have them; defer until requested.
///
/// Roborev 183 #3: 60s TTL cache. /me fields (mail, displayName, id) are
/// essentially session-immutable, so the cache mostly exists to remove
/// the extra RTT on every send-with-from_addr — not to track changing
/// state.
pub async fn get_identities(
    session: &OutlookSession,
) -> Result<Vec<crate::types::Identity>, Error> {
    {
        let cache = session.identity_cache.lock().await;
        if let Some(entry) = cache.as_ref()
            && entry.fetched_at.elapsed() < IDENTITY_CACHE_TTL
        {
            return Ok(entry.identities.clone());
        }
    }

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

    let identities = vec![crate::types::Identity {
        id,
        email,
        name: display_name,
    }];
    *session.identity_cache.lock().await = Some(IdentityCacheEntry {
        fetched_at: Instant::now(),
        identities: identities.clone(),
    });
    Ok(identities)
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

    // Update cache cursor for next-page following. Cap enforcement
    // (roborev 179 #3): evict an arbitrary entry if we'd exceed the cap.
    // Re-issuing the evicted query is just one Graph round-trip — cheap.
    let mut cache = session.page_cache.lock().await;
    if cache.len() >= MAX_PAGE_CACHE_ENTRIES
        && !cache.contains_key(&cache_key)
        && let Some(victim_key) = cache.keys().next().cloned()
    {
        cache.remove(&victim_key);
    }
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
        // Roborev 179 #1: Graph rejects $orderby combined with $search
        // ($search results are relevance-ranked). Skipping $orderby when
        // $search is set is the load-bearing fix; results then come back
        // in Graph's default order (relevance for $search, server-default
        // otherwise — usually receivedDateTime desc).
        //
        // Roborev 179 #2 policy: when both $filter and $search are set,
        // we emit both. Graph's docs say this is supported on
        // /me/messages with restrictions; if a tenant rejects the
        // combination, the user-visible error from classify_outlook_error
        // makes it visible. Switching to client-side filtering is the
        // documented fallback; defer until reports come in.
        if odata.search.is_none() {
            q.append_pair("$orderby", "receivedDateTime desc");
        }
    }
    url.to_string()
}

/// Fetch full message data for each ID in parallel. Uses `$expand=attachments`
/// so attachment metadata comes back in the same response.
/// Cap on concurrent in-flight Graph requests from a single `get_emails`
/// call. Graph throttles aggressively (HTTP 429 with `Retry-After`);
/// fanning out 200 requests at once is a recipe for a 429 storm.
/// 5 is conservative — matches what Gmail's get_emails effectively
/// achieves via tokio's natural connection-pool serialization.
/// Roborev 179 #5.
const GET_EMAILS_MAX_CONCURRENCY: usize = 5;

pub async fn get_emails(
    session: &OutlookSession,
    ids: &[String],
    fetch_body: bool,
) -> Result<Vec<crate::types::Email>, Error> {
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    let token = access_token(session).await?;
    let sem = std::sync::Arc::new(tokio::sync::Semaphore::new(GET_EMAILS_MAX_CONCURRENCY));
    let mut join_set = tokio::task::JoinSet::new();
    for (idx, id) in ids.iter().enumerate() {
        let client = session.client.clone();
        let token = token.clone();
        let id = id.clone();
        let sem = sem.clone();
        join_set.spawn(async move {
            // Acquire holds the permit for the duration of the request.
            let _permit = sem
                .acquire()
                .await
                .map_err(|e| Error::Internal(format!("get_emails semaphore: {e}")))?;
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
// Phase 4 Milestone B — async write paths
// =============================================================================
//
// Each mutation invalidates BOTH folder_cache (unread counts shift) and
// page_cache (cached @odata.nextLink URLs would otherwise skip the moved
// or modified messages). Roborev 179 #3 codified the page_cache
// invalidation requirement.

/// Invalidate both caches that a mutation could stale. Single helper so
/// no mutation forgets one half.
async fn invalidate_caches_after_mutation(session: &OutlookSession) {
    invalidate_folder_cache(session).await;
    invalidate_page_cache(session).await;
}

/// PATCH a message with the given JSON body. Shared by mark_read/unread
/// and toggle_flag (and any future single-message PATCH mutation).
async fn patch_message(
    session: &OutlookSession,
    msg_id: &str,
    body: &serde_json::Value,
    op: &str,
) -> Result<bool, Error> {
    let token = access_token(session).await?;
    let encoded = crate::provider_utils::encode_path_segment(msg_id);
    let url = format!("{GRAPH_BASE}/me/messages/{encoded}");
    let resp = session
        .client
        .patch(&url)
        .bearer_auth(&token)
        .json(body)
        .send()
        .await?;
    let status = resp.status();
    if !status.is_success() {
        let text = resp.text().await.unwrap_or_default();
        return Err(classify_outlook_error(
            &format!("{op} {msg_id}"),
            status,
            &text,
        ));
    }
    invalidate_caches_after_mutation(session).await;
    Ok(true)
}

pub async fn mark_read(session: &OutlookSession, msg_id: &str) -> Result<bool, Error> {
    patch_message(session, msg_id, &mark_read_body(true), "mark_read").await
}

pub async fn mark_unread(session: &OutlookSession, msg_id: &str) -> Result<bool, Error> {
    patch_message(session, msg_id, &mark_read_body(false), "mark_unread").await
}

/// Toggle the flag status by reading the current state then PATCHing the
/// opposite. Two API calls; same TOCTOU caveat as Gmail's toggle_flag.
pub async fn toggle_flag(session: &OutlookSession, msg_id: &str) -> Result<bool, Error> {
    let token = access_token(session).await?;
    let encoded = crate::provider_utils::encode_path_segment(msg_id);
    let url = format!("{GRAPH_BASE}/me/messages/{encoded}?$select=flag");
    let resp = session.client.get(&url).bearer_auth(&token).send().await?;
    let status = resp.status();
    if !status.is_success() {
        let text = resp.text().await.unwrap_or_default();
        return Err(classify_outlook_error(
            &format!("toggle_flag.get {msg_id}"),
            status,
            &text,
        ));
    }
    let parsed: serde_json::Value = resp.json().await?;
    // Roborev 180 #7: messages that have never been flagged may return
    // `{}` or omit the `flag` key entirely from $select=flag. Treat
    // missing == not-flagged so toggling defaults to flagging (the
    // user's likely intent — they clicked the star icon).
    let currently_flagged = parsed["flag"]["flagStatus"].as_str() == Some("flagged");
    patch_message(
        session,
        msg_id,
        &flag_body(!currently_flagged),
        "toggle_flag",
    )
    .await
}

/// Move a message to the destination folder. Pure routing happens via
/// `move_plan_outlook`; the HTTP request is `POST .../move`. On 404 from
/// Graph (folder missing — most commonly the Archive folder on accounts
/// that haven't enabled it), surface a `BadRequest` with actionable text
/// rather than a silent failure (roborev 175 / plan top-5 finding).
async fn move_message(
    session: &OutlookSession,
    msg_id: &str,
    destination: &str,
    op: &str,
) -> Result<bool, Error> {
    let token = access_token(session).await?;
    let encoded = crate::provider_utils::encode_path_segment(msg_id);
    let url = format!("{GRAPH_BASE}/me/messages/{encoded}/move");
    let resp = session
        .client
        .post(&url)
        .bearer_auth(&token)
        .json(&move_body(destination))
        .send()
        .await?;
    let status = resp.status();
    if !status.is_success() {
        let text = resp.text().await.unwrap_or_default();
        // Roborev 180 #8: the destination-string match is intentional —
        // archive() is the only call path that uses the literal "archive"
        // well-known name. move_to_mailbox with an opaque Archive-folder
        // ID would route through the role check earlier; we never get
        // here for that path with a non-archive 404.
        if status == reqwest::StatusCode::NOT_FOUND && destination == "archive" {
            return Err(archive_404_message());
        }
        return Err(classify_outlook_error(
            &format!("{op} {msg_id} -> {destination}"),
            status,
            &text,
        ));
    }
    invalidate_caches_after_mutation(session).await;
    Ok(true)
}

pub async fn archive(session: &OutlookSession, msg_id: &str) -> Result<bool, Error> {
    move_message(session, msg_id, "archive", "archive").await
}

pub async fn trash(session: &OutlookSession, msg_id: &str) -> Result<bool, Error> {
    move_message(session, msg_id, "deleteditems", "trash").await
}

pub async fn move_to_mailbox(
    session: &OutlookSession,
    msg_id: &str,
    mailbox_id: &str,
) -> Result<bool, Error> {
    // First check: literal well-known-name match (drafts/sentitems/junkemail).
    // Catches the common case where the frontend passes "drafts" directly.
    let destination = match move_plan_outlook(mailbox_id) {
        MovePlanOutlook::Reject(reason) => return Err(Error::BadRequest(reason.into())),
        MovePlanOutlook::Move { destination } => destination,
    };
    // Second check (roborev 180 #6): the frontend might pass an opaque
    // Graph folder ID for a system folder rather than the well-known
    // name. Resolve via folder_cache and apply the same rejection rules
    // by role. Graceful: if folder lookup fails, fall through to attempt
    // the move and let Graph return a real error.
    if let Ok(folders) = get_mailboxes(session).await
        && let Some(folder) = folders.iter().find(|f| f.id == mailbox_id)
        && let Some(reason) = role_rejects_move(folder.role.as_deref())
    {
        return Err(Error::BadRequest(reason.into()));
    }
    move_message(session, msg_id, &destination, "move").await
}

/// Archive a batch of messages via Graph's `/$batch` endpoint, chunked at
/// the documented 20-per-batch cap.
///
/// Contract (roborev 180 #3): all-or-nothing per call. On any chunk
/// failure (chunk-level HTTP error, missing/malformed `responses` array,
/// or any per-item non-2xx) returns `Err` describing the failure. Cache
/// invalidation has been applied for chunks that completed before the
/// failure, so the caller's next read sees fresh state for the partial
/// move. The returned `usize` on success is the total count submitted
/// across all chunks (== msg_ids.len()).
///
/// If any per-item 404 fires when the destination is the Archive folder
/// (typically on accounts without one set up), returns the actionable
/// "create Archive folder" message that single-message `archive()`
/// produces — roborev 180 #2.
pub async fn archive_batch(session: &OutlookSession, msg_ids: &[String]) -> Result<usize, Error> {
    if msg_ids.is_empty() {
        return Ok(0);
    }
    let token = access_token(session).await?;
    let chunks = chunk_batch_requests(msg_ids);
    let mut succeeded = 0usize;
    for chunk in chunks {
        let body = build_batch_archive_body(&chunk);
        let resp = session
            .client
            .post(format!("{GRAPH_BASE}/$batch"))
            .bearer_auth(&token)
            .json(&body)
            .send()
            .await?;
        let status = resp.status();
        if !status.is_success() {
            let text = resp.text().await.unwrap_or_default();
            // Roborev 180 #1: invalidate caches for chunks that did
            // complete before this failure so the caller's next read
            // doesn't see stale counts.
            invalidate_caches_after_mutation(session).await;
            return Err(classify_outlook_error("$batch.archive", status, &text));
        }
        // Per-item failures live in `responses[].status`; Graph's
        // overall HTTP status is still 200. parse_batch_response
        // (pure, tested) extracts the outcomes — roborev 180 #4 + #5.
        let parsed: serde_json::Value = resp.json().await?;
        let outcome = parse_batch_response(&parsed, chunk.len()).map_err(|e| {
            // Roborev 180 #1: invalidate before bailing.
            // (Async closure restriction: can't await here, so do it
            // by hand after match below.)
            Error::Internal(format!("Outlook $batch archive: {e}"))
        });
        // Hoist the invalidate-before-bail because async closures can't
        // .await inside map_err.
        let outcome = match outcome {
            Err(e) => {
                invalidate_caches_after_mutation(session).await;
                return Err(e);
            }
            Ok(o) => o,
        };
        if !outcome.errors.is_empty() {
            invalidate_caches_after_mutation(session).await;
            // Archive-folder-missing wins: actionable user message.
            if outcome.any_404 {
                return Err(archive_404_message());
            }
            return Err(Error::BadRequest(format!(
                "Outlook $batch archive: {} of {} failed: {}",
                outcome.errors.len(),
                chunk.len(),
                outcome.errors.join(" | ")
            )));
        }
        succeeded += outcome.succeeded;
        // Roborev 180 #1: per-chunk invalidation so a subsequent failure
        // can't leave earlier chunks' state stale.
        invalidate_caches_after_mutation(session).await;
    }
    Ok(succeeded)
}

/// Build a Graph `/$batch` request body for archiving a chunk of msg IDs.
/// Each entry POSTs to the per-message /move endpoint with destinationId
/// "archive". Pure — extracted so the JSON shape is unit-testable.
fn build_batch_archive_body(msg_ids: &[&str]) -> serde_json::Value {
    let requests: Vec<serde_json::Value> = msg_ids
        .iter()
        .enumerate()
        .map(|(i, id)| {
            let encoded = crate::provider_utils::encode_path_segment(id);
            serde_json::json!({
                "id": (i + 1).to_string(),
                "method": "POST",
                "url": format!("/me/messages/{encoded}/move"),
                "headers": { "Content-Type": "application/json" },
                "body": { "destinationId": "archive" }
            })
        })
        .collect();
    serde_json::json!({ "requests": requests })
}

// =============================================================================
// Phase 4 Milestone B — download_blob
// =============================================================================

/// Download attachment bytes by `BlobRef::OutlookAttachment`. Graph's
/// `/$value` endpoint returns raw bytes with `Content-Type` set from the
/// attachment's stored contentType — more accurate than guessing from
/// the filename extension (which Gmail has to do).
///
/// The `_filename` parameter is intentionally ignored here — Graph
/// returns the truth in the `Content-Type` response header, so we don't
/// need to extension-guess. The param is part of the provider dispatch
/// signature (Gmail uses it; symmetric trait shape).
/// Roborev 180 #10.
pub async fn download_blob(
    session: &OutlookSession,
    blob_id: &str,
    _filename: &str,
) -> Result<(String, Vec<u8>), Error> {
    let blob_ref = crate::types::BlobRef::parse(blob_id)?;
    let (msg_id, att_id) = match blob_ref {
        crate::types::BlobRef::OutlookAttachment { msg_id, att_id } => (msg_id, att_id),
        crate::types::BlobRef::Synthetic(_) => {
            return Err(Error::BadRequest(
                "synthetic blob_id passed to outlook::download_blob — compose \
                 uploads aren't downloadable until they're sent"
                    .into(),
            ));
        }
        crate::types::BlobRef::GmailAttachment { .. } => {
            return Err(Error::BadRequest(
                "gmail blob_id passed to outlook::download_blob — wrong provider".into(),
            ));
        }
    };

    let token = access_token(session).await?;
    let msg_enc = crate::provider_utils::encode_path_segment(&msg_id);
    let att_enc = crate::provider_utils::encode_path_segment(&att_id);
    let url = format!("{GRAPH_BASE}/me/messages/{msg_enc}/attachments/{att_enc}/$value");
    let resp = session.client.get(&url).bearer_auth(&token).send().await?;
    let status = resp.status();
    if !status.is_success() {
        let text = resp.text().await.unwrap_or_default();
        return Err(classify_outlook_error(
            "messages.attachments.get/$value",
            status,
            &text,
        ));
    }
    let content_type = resp
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("application/octet-stream")
        .to_string();
    let bytes = resp.bytes().await?.to_vec();
    Ok((content_type, bytes))
}

// =============================================================================
// Phase 4 Milestone C — send + compose
// =============================================================================

use crate::types::{EmailSubmission, Identity};

/// Which Graph API path to use to send `sub`. Top-5 greats finding plus
/// roborev 181 #1 refinement:
/// - `NewMail` always uses sendMail (attachments inline OK).
/// - `ReplyOneShot` (was `ReplyNoAttachments`) uses `/reply` in 1 RTT
///   ONLY when the user composed HTML AND there are no attachments.
///   Graph's `/reply` treats `comment` as HTML; a plain-text body would
///   have its newlines collapsed by HTML rendering.
/// - `ReplyWithAttachments` runs the 3-call createReply → PATCH → send
///   dance for everything else (plain-text replies, attachments, or
///   Cc/Bcc additions). The PATCH can set body.contentType="text"
///   correctly.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub(crate) enum SendPath {
    NewMail,
    ReplyOneShot,
    ReplyWithAttachments,
}

/// Pure dispatch on (in_reply_to, html_body, attachments). Treats `None`
/// and empty `attachments` identically (defensive against frontend habits).
pub(crate) fn pick_send_path(sub: &EmailSubmission) -> SendPath {
    match (&sub.in_reply_to, &sub.html_body, sub.attachments.is_empty()) {
        (None, _, _) => SendPath::NewMail,
        // Reply with HTML body + no attachments: 1-RTT /reply path.
        (Some(_), Some(_), true) => SendPath::ReplyOneShot,
        // Plain-text reply OR has attachments: 3-call path so we can
        // PATCH the draft with contentType="text" / attachments inlined.
        (Some(_), _, _) => SendPath::ReplyWithAttachments,
    }
}

/// Pure wiring helper (roborev 183 #5): decides whether and how
/// `send_email` should invoke the picker. Splitting this out from the
/// async `send_email` body lets us pin the routing logic with unit
/// tests instead of relying on HTTP mocking.
///
/// - Reply paths return `None` unconditionally — Graph inherits From
///   from the parent message server-side, so any display name we
///   resolve would be silently dropped anyway (and Graph doesn't
///   honor `from.emailAddress.name` on the reply endpoints either).
/// - `NewMail` with no `from_addr` returns `None` — there's no From
///   to label.
/// - Everything else delegates to `pick_outlook_display_name`.
fn pick_from_name_for_send_path(
    path: SendPath,
    from_addr: Option<&str>,
    identity_id_override: Option<&str>,
    identities: &[Identity],
) -> Option<String> {
    match (path, from_addr) {
        (SendPath::NewMail, Some(addr)) if !addr.is_empty() => {
            pick_outlook_display_name(identities, addr, identity_id_override)
        }
        _ => None,
    }
}

/// Pick the display name to set on Graph's `from.emailAddress.name`.
///
/// Pure — caller fetches identities once and passes them in. Returns the
/// display name to attach to From, or `None` to leave it unset (Graph
/// then renders the authenticated user's default).
///
/// Discipline (mirrors `gmail::pick_identity_display_name`): if
/// `identity_id_override` is `Some(id)` but doesn't match any known
/// identity, return `None` and emit a `tracing::warn!` — refuses to
/// silently fall back to a different identity, because attaching the
/// wrong name to the From line is a worse failure than having no name
/// at all.
///
/// Outlook's identity list is single-element today (just `/me`), so the
/// no-override / no-match path also returns `None`: with only one
/// identity, a non-matching `from_addr` means the caller is trying to
/// send-as something they don't own. Attaching the primary identity's
/// name would mislabel From. (Gmail can afford a first-identity fallback
/// because its sendAs list often has the user's many real addresses; we
/// don't have that luxury yet.) If alias enumeration ever lands, this
/// helper's signature already takes a slice and the fallback policy can
/// be revisited without changing call sites.
fn pick_outlook_display_name(
    identities: &[Identity],
    from_addr: &str,
    identity_id_override: Option<&str>,
) -> Option<String> {
    let chosen = match identity_id_override {
        Some(id) => match identities.iter().find(|i| i.id == id) {
            None => {
                tracing::warn!(
                    identity_id_override = %id,
                    "Outlook send: identity_id_override does not match any \
                     identity; sending without display name to avoid \
                     mislabeling From"
                );
                None
            }
            Some(ident) if ident.email.eq_ignore_ascii_case(from_addr) => Some(ident),
            // Roborev 183 #1 defense-in-depth: the override id is real
            // but its email doesn't match from_addr — attaching that
            // identity's name to a different From address would
            // mislabel just as badly as the unknown-id case.
            Some(ident) => {
                tracing::warn!(
                    identity_id_override = %id,
                    identity_email = %ident.email,
                    from_addr,
                    "Outlook send: identity_id_override id matches but its \
                     email does not match from_addr; sending without \
                     display name to avoid mislabeling From"
                );
                None
            }
        },
        None => identities
            .iter()
            .find(|i| i.email.eq_ignore_ascii_case(from_addr)),
    };
    chosen.map(|i| i.name.clone()).filter(|n| !n.is_empty())
}

/// Build a Graph `Message` resource with an optional `from` field.
/// Used for shared-mailbox / send-as scenarios where the caller wants to
/// pick the From address (roborev 181 #5). For personal mailboxes pass
/// `None` and Graph defaults to the authenticated user.
///
/// Empty `from_addr` is treated as `None` (defensive against the
/// frontend always sending the user's primary address even when no
/// override is needed).
///
/// Body: HTML if present, else text. Graph picks contentType from the
/// `body.contentType` field, not a header.
///
/// Attachments inline as `fileAttachment` entries with base64-encoded
/// `contentBytes` (Graph has no standalone blob store — bytes must be
/// in the message body itself).
pub(crate) fn build_graph_message_with_from_identity(
    sub: &EmailSubmission,
    resolved_attachments: &[(String, String, Vec<u8>)],
    from_addr: Option<&str>,
    from_name: Option<&str>,
) -> serde_json::Value {
    fn recipients(addrs: &[String]) -> serde_json::Value {
        let arr: Vec<serde_json::Value> = addrs
            .iter()
            .map(|a| serde_json::json!({ "emailAddress": { "address": a } }))
            .collect();
        serde_json::json!(arr)
    }

    // Pick HTML when both are present — the user composed HTML and the
    // text_body is the plain-text fallback we'd send separately on JMAP.
    // Graph's single body field forces us to pick one; pick the richer.
    let (content_type, content) = match &sub.html_body {
        Some(html) => ("html", html.clone()),
        None => ("text", sub.text_body.clone()),
    };

    let mut msg = serde_json::json!({
        "subject": sub.subject,
        "body": { "contentType": content_type, "content": content },
        "toRecipients": recipients(&sub.to),
    });

    if !sub.cc.is_empty() {
        msg["ccRecipients"] = recipients(&sub.cc);
    }
    if let Some(bcc) = &sub.bcc
        && !bcc.is_empty()
    {
        msg["bccRecipients"] = recipients(bcc);
    }
    // Roborev 181 #5: send-as / shared-mailbox support. Only set if the
    // caller explicitly asked for a non-default sender. Milestone D.1:
    // also thread the picker-resolved display name onto
    // `from.emailAddress.name` when present.
    if let Some(addr) = from_addr
        && !addr.is_empty()
    {
        let mut email_addr = serde_json::json!({ "address": addr });
        if let Some(name) = from_name
            && !name.is_empty()
        {
            email_addr["name"] = serde_json::Value::String(name.to_string());
        }
        msg["from"] = serde_json::json!({ "emailAddress": email_addr });
    }

    if !resolved_attachments.is_empty() {
        msg["attachments"] = build_graph_attachments_array(resolved_attachments);
    }

    msg
}

/// Minimal PATCH body for the 3-call reply path. Roborev 181 #6:
/// createReply pre-populates the draft's `subject`, `toRecipients`,
/// `inReplyTo`, `references`, `conversationId`, and threading
/// `internetMessageHeaders`. PATCHing those again would clobber the
/// correct values with whatever the frontend happened to send.
///
/// What we DO set (the things the user actually edited):
/// - `body` with `contentType` text vs html so plain-text replies
///   preserve newlines (the root of roborev 181 #1).
/// - `ccRecipients` / `bccRecipients` when the user added them.
/// - `attachments` when resolved.
pub(crate) fn build_graph_reply_patch_body(
    sub: &EmailSubmission,
    resolved_attachments: &[(String, String, Vec<u8>)],
) -> serde_json::Value {
    fn recipients(addrs: &[String]) -> serde_json::Value {
        let arr: Vec<serde_json::Value> = addrs
            .iter()
            .map(|a| serde_json::json!({ "emailAddress": { "address": a } }))
            .collect();
        serde_json::json!(arr)
    }
    let (content_type, content) = match &sub.html_body {
        Some(html) => ("html", html.clone()),
        None => ("text", sub.text_body.clone()),
    };
    let mut body = serde_json::json!({
        "body": { "contentType": content_type, "content": content },
    });
    if !sub.cc.is_empty() {
        body["ccRecipients"] = recipients(&sub.cc);
    }
    if let Some(bcc) = &sub.bcc
        && !bcc.is_empty()
    {
        body["bccRecipients"] = recipients(bcc);
    }
    if !resolved_attachments.is_empty() {
        body["attachments"] = build_graph_attachments_array(resolved_attachments);
    }
    body
}

/// Build the `attachments[]` JSON array shared by sendMail and draft
/// PATCH builders. Pure — extracted to keep both call sites consistent
/// (one place to fix Graph schema drift).
fn build_graph_attachments_array(
    resolved_attachments: &[(String, String, Vec<u8>)],
) -> serde_json::Value {
    use base64::Engine;
    let atts: Vec<serde_json::Value> = resolved_attachments
        .iter()
        .map(|(name, mime, bytes)| {
            serde_json::json!({
                "@odata.type": "#microsoft.graph.fileAttachment",
                "name": name,
                "contentType": mime,
                "contentBytes": base64::engine::general_purpose::STANDARD.encode(bytes),
            })
        })
        .collect();
    serde_json::json!(atts)
}

/// Stash compose-time bytes in the session's synthetic-blob cache; return
/// the `BlobRef::Synthetic(uuid).to_string()` for the frontend to embed in
/// the EmailSubmission. Cap-enforced (count + per-blob + aggregate) so a
/// misbehaving client can't grow memory unbounded. Mirror of Gmail's
/// upload_blob.
pub async fn upload_blob(
    session: &OutlookSession,
    content_type: &str,
    body: &[u8],
) -> Result<(String, i64), Error> {
    if body.len() > MAX_BLOB_BYTES {
        return Err(Error::BadRequest(format!(
            "Outlook attachment too large: {} bytes (limit: {} MiB). \
             Larger uploads need Graph's upload-session flow (not yet wired).",
            body.len(),
            MAX_BLOB_BYTES / 1024 / 1024
        )));
    }
    let mut cache = session.upload_cache.lock().await;
    if cache.len() >= UPLOAD_CACHE_CAP {
        return Err(Error::BadRequest(format!(
            "Outlook upload cache full ({UPLOAD_CACHE_CAP} entries). \
             Cancel or send pending drafts before attaching more files."
        )));
    }
    let current_total: usize = cache.values().map(|(_, b)| b.len()).sum();
    if current_total + body.len() > MAX_UPLOAD_CACHE_BYTES {
        return Err(Error::BadRequest(format!(
            "Outlook upload cache aggregate size would exceed {} MiB \
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

/// Read a blob's bytes for inclusion in a send body without mutating the
/// cache. Synthetic blobs are cloned from upload_cache; OutlookAttachment
/// blobs are re-fetched via download_blob (for the "reply with original
/// attachment" path); Gmail/wrong-prefix blobs are rejected.
///
/// Cache draining happens after a successful send via
/// `drain_consumed_synthetic_blobs` so partial-build failures don't lose
/// already-uploaded bytes (the same correctness property that motivated
/// Gmail's roborev 176 #1 fix).
async fn peek_blob_bytes(
    session: &OutlookSession,
    blob_id: &str,
) -> Result<(String, Vec<u8>), Error> {
    let blob_ref = crate::types::BlobRef::parse(blob_id)?;
    match blob_ref {
        crate::types::BlobRef::Synthetic(id) => {
            let cache = session.upload_cache.lock().await;
            cache.get(&id).cloned().ok_or_else(|| {
                Error::BadRequest(format!(
                    "Outlook synthetic blob {id} not found (already consumed or session restarted)"
                ))
            })
        }
        crate::types::BlobRef::OutlookAttachment { .. } => {
            // Re-fetch the original attachment bytes (reply-with-original
            // path). download_blob re-parses the blob_id internally.
            download_blob(session, blob_id, "").await
        }
        crate::types::BlobRef::GmailAttachment { .. } => Err(Error::BadRequest(
            "gmail blob_id passed to outlook::peek_blob_bytes — wrong provider".into(),
        )),
    }
}

/// Drop synthetic-blob entries from `upload_cache` after a successful
/// send. Idempotent; non-synthetic and missing entries are silently
/// skipped. Mirrors Gmail's drain semantics.
async fn drain_consumed_synthetic_blobs(
    session: &OutlookSession,
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

/// Resolve every attachment in the EmailSubmission to its (name, mime,
/// bytes) triple via `peek_blob_bytes`. Failure on any attachment aborts
/// the whole send — the upload cache stays intact so the user can retry.
async fn resolve_all_attachments(
    session: &OutlookSession,
    attachments: &[crate::types::Attachment],
) -> Result<Vec<(String, String, Vec<u8>)>, Error> {
    let mut out = Vec::with_capacity(attachments.len());
    for att in attachments {
        let (mime, bytes) = peek_blob_bytes(session, &att.blob_id).await?;
        // Prefer the attachment's declared mime_type; fall back to what
        // peek returned. Matches Gmail's discipline.
        // Roborev 181 #8: surface empty-mime fallbacks in logs so a
        // frontend bug doesn't silently corrupt content-types in Sent.
        let final_mime = if att.mime_type.is_empty() {
            tracing::warn!(
                attachment_name = %att.name,
                fallback_mime = %mime,
                "Outlook send: attachment mime_type was empty; falling back \
                 to the resolved blob's MIME"
            );
            mime
        } else {
            att.mime_type.clone()
        };
        out.push((att.name.clone(), final_mime, bytes));
    }
    Ok(out)
}

/// Send via the chosen Graph path. Top-5 greats finding: branch on
/// `pick_send_path` so no-attachment replies take 1 RTT, with-attachment
/// replies eat the 3-RTT createReply → PATCH → send dance. Orphan-draft
/// cleanup on the 3-call path so a mid-flight failure doesn't strand a
/// draft in the user's Drafts folder.
///
/// On success: drain consumed synthetic blobs (cache no longer needs the
/// bytes) and invalidate folder_cache + page_cache (the sent message
/// lands in `sentitems` and would otherwise show stale cursors).
pub async fn send_email(
    session: &OutlookSession,
    sub: &EmailSubmission,
    from_addr: Option<&str>,
    identity_id_override: Option<&str>,
) -> Result<Option<String>, Error> {
    // Resolve attachments BEFORE deciding the send path — even the
    // ReplyOneShot branch validates the upload cache state (resolves to
    // Ok(vec![]) cheaply when there are none).
    let resolved = resolve_all_attachments(session, &sub.attachments).await?;
    let path = pick_send_path(sub);
    // Roborev 181 #10: replace expect() with ok_or_else so a future
    // contributor synthesizing a SendPath from outside pick_send_path
    // doesn't panic — graceful degradation to Internal error instead.
    let parent = || -> Result<&str, Error> {
        sub.in_reply_to.as_deref().ok_or_else(|| {
            Error::Internal(
                "SendPath reply variant produced with no in_reply_to — \
                 pick_send_path invariant violated"
                    .into(),
            )
        })
    };
    // Roborev 183 #2: warn when the user picked a non-default identity
    // on a path that can't honor it. Two such cases: replies (Graph
    // inherits From from the parent message), and NewMail with no
    // from_addr (nothing to label). Silently dropping disagrees with
    // the user — at minimum let the operator see the mismatch.
    if identity_id_override.is_some() && !matches!(path, SendPath::NewMail) {
        tracing::warn!(
            ?identity_id_override,
            "Outlook send: identity_id_override ignored on reply paths — \
             Graph inherits From from the parent message server-side"
        );
    } else if identity_id_override.is_some()
        && matches!(path, SendPath::NewMail)
        && from_addr.is_none()
    {
        tracing::warn!(
            ?identity_id_override,
            "Outlook send: identity_id_override set but from_addr is empty — \
             frontend should send both; ignoring override"
        );
    }
    // Resolve display name. Only NewMail with from_addr ever invokes
    // the picker (see pick_from_name_for_send_path). Roborev 183 #3:
    // identities come from the 60s-TTL cache to avoid an extra
    // /me RTT on every send. Roborev 183 #4: if the load fails and we
    // had an override to honor, emit a tracing::error so the operator
    // sees the real cause rather than the misleading "override unknown"
    // warn the picker would emit on an empty identity list.
    let identities = if matches!(path, SendPath::NewMail) && from_addr.is_some() {
        match get_identities(session).await {
            Ok(ids) => ids,
            Err(e) => {
                if identity_id_override.is_some() {
                    tracing::error!(
                        error = %e,
                        ?identity_id_override,
                        "Outlook send: failed to load identities; cannot \
                         honor identity_id_override on this send"
                    );
                }
                Vec::new()
            }
        }
    } else {
        Vec::new()
    };
    let from_name =
        pick_from_name_for_send_path(path, from_addr, identity_id_override, &identities);
    let new_id = match path {
        SendPath::NewMail => {
            send_new_mail(session, sub, &resolved, from_addr, from_name.as_deref()).await?
        }
        SendPath::ReplyOneShot => send_reply_one_shot(session, sub, parent()?).await?,
        SendPath::ReplyWithAttachments => {
            send_reply_with_attachments(session, sub, parent()?, &resolved).await?
        }
    };
    drain_consumed_synthetic_blobs(session, &sub.attachments).await;
    invalidate_caches_after_mutation(session).await;
    Ok(new_id)
}

/// `POST /me/sendMail` with the full Graph Message inline. Used for new
/// mail (no `in_reply_to`) regardless of attachments — sendMail accepts
/// attachments inline. Returns `Ok(None)` because sendMail returns 202
/// Accepted with no body and no message ID; Graph doesn't tell us the
/// stored Sent-folder message ID (a known Graph limitation).
async fn send_new_mail(
    session: &OutlookSession,
    sub: &EmailSubmission,
    resolved: &[(String, String, Vec<u8>)],
    from_addr: Option<&str>,
    from_name: Option<&str>,
) -> Result<Option<String>, Error> {
    let token = access_token(session).await?;
    let message = build_graph_message_with_from_identity(sub, resolved, from_addr, from_name);
    // saveToSentItems defaults to true on the Graph side but spec it
    // explicitly so the wire shape doesn't depend on tenant defaults.
    let body = serde_json::json!({
        "message": message,
        "saveToSentItems": true,
    });
    let resp = session
        .client
        .post(format!("{GRAPH_BASE}/me/sendMail"))
        .bearer_auth(&token)
        .json(&body)
        .send()
        .await?;
    let status = resp.status();
    if !status.is_success() {
        let text = resp.text().await.unwrap_or_default();
        return Err(classify_outlook_error("sendMail", status, &text));
    }
    Ok(None)
}

/// `POST /me/messages/{parent}/reply` — single call, Graph threads it
/// server-side via the parent message ID, no draft state. Body is
/// `{ "comment": "<html>" }` — Graph treats `comment` as HTML and
/// inherits headers/recipients from the parent.
///
/// Roborev 181 #1: this path is reachable only when `pick_send_path`
/// returns `ReplyOneShot`, which requires `html_body.is_some()` AND no
/// attachments — plain-text replies route through the 3-call path so
/// their newlines don't get HTML-collapsed.
///
/// If the EmailSubmission carries new Cc/Bcc (which the 1-call path
/// can't apply), fall back to the 3-call createReply path so the PATCH
/// can set those fields.
async fn send_reply_one_shot(
    session: &OutlookSession,
    sub: &EmailSubmission,
    parent_msg_id: &str,
) -> Result<Option<String>, Error> {
    if !sub.cc.is_empty() || sub.bcc.as_ref().is_some_and(|v| !v.is_empty()) {
        return send_reply_with_attachments(session, sub, parent_msg_id, &[]).await;
    }
    let token = access_token(session).await?;
    let encoded = crate::provider_utils::encode_path_segment(parent_msg_id);
    let url = format!("{GRAPH_BASE}/me/messages/{encoded}/reply");
    // Invariant from pick_send_path: html_body is Some on this branch.
    // Defensive: if somehow not, fall back to plain text wrapped in <pre>
    // to preserve formatting rather than corrupting it.
    let comment = match &sub.html_body {
        Some(html) => html.clone(),
        None => format!(
            "<pre>{}</pre>",
            sub.text_body
                .replace('&', "&amp;")
                .replace('<', "&lt;")
                .replace('>', "&gt;")
        ),
    };
    let body = serde_json::json!({ "comment": comment });
    let resp = session
        .client
        .post(&url)
        .bearer_auth(&token)
        .json(&body)
        .send()
        .await?;
    let status = resp.status();
    if !status.is_success() {
        let text = resp.text().await.unwrap_or_default();
        return Err(classify_outlook_error(
            &format!("reply {parent_msg_id}"),
            status,
            &text,
        ));
    }
    Ok(None)
}

/// Three-call dance for replies that need attachments (or new Cc/Bcc):
/// `POST .../createReply` → returns draft → `PATCH draft` with body +
/// attachments + recipients → `POST draft/send`.
///
/// Orphan-draft cleanup: if PATCH or send fails after createReply
/// succeeded, attempt `DELETE draft` so we don't leave a confusing draft
/// in the user's Drafts folder. If cleanup itself fails, the user-facing
/// error mentions the draft.
async fn send_reply_with_attachments(
    session: &OutlookSession,
    sub: &EmailSubmission,
    parent_msg_id: &str,
    resolved: &[(String, String, Vec<u8>)],
) -> Result<Option<String>, Error> {
    let token = access_token(session).await?;
    let encoded_parent = crate::provider_utils::encode_path_segment(parent_msg_id);

    // 1. createReply → draft.id
    let create_url = format!("{GRAPH_BASE}/me/messages/{encoded_parent}/createReply");
    let resp = session
        .client
        .post(&create_url)
        .bearer_auth(&token)
        .send()
        .await?;
    let status = resp.status();
    if !status.is_success() {
        let text = resp.text().await.unwrap_or_default();
        return Err(classify_outlook_error(
            &format!("createReply {parent_msg_id}"),
            status,
            &text,
        ));
    }
    let draft: serde_json::Value = resp.json().await?;
    let draft_id = draft["id"]
        .as_str()
        .ok_or_else(|| Error::Internal("createReply response missing draft id".into()))?
        .to_string();

    // 2. PATCH draft with full body + attachments + recipients. We use
    //    build_graph_message and then trim the wrapper so we PATCH the
    //    Message resource directly (not wrapped in {message: ...}).
    match patch_and_send_draft(session, &token, &draft_id, sub, resolved).await {
        // Roborev 181 #2: return Ok(None) for parity with send_new_mail.
        // After /send, the draft is consumed and gets a new ID in Sent
        // Items that Graph doesn't return; surfacing the now-defunct
        // draft_id would 404 on any caller trying to use it.
        Ok(()) => Ok(None),
        Err(e) => {
            // Roborev 181 #3: pure helper maps cleanup outcome to the
            // final error so the wording — especially the AlreadyGone
            // "check Sent Items" warning — stays unit-pinned.
            let cleanup = delete_draft(session, &token, &draft_id).await;
            Err(format_send_failure_with_cleanup(e, cleanup, &draft_id))
        }
    }
}

async fn patch_and_send_draft(
    session: &OutlookSession,
    token: &str,
    draft_id: &str,
    sub: &EmailSubmission,
    resolved: &[(String, String, Vec<u8>)],
) -> Result<(), Error> {
    let encoded_draft = crate::provider_utils::encode_path_segment(draft_id);
    // Roborev 181 #6: use the minimal PATCH builder so we don't clobber
    // subject/toRecipients/threading headers that createReply
    // pre-populated correctly.
    let patch_body = build_graph_reply_patch_body(sub, resolved);
    let patch_url = format!("{GRAPH_BASE}/me/messages/{encoded_draft}");
    let resp = session
        .client
        .patch(&patch_url)
        .bearer_auth(token)
        .json(&patch_body)
        .send()
        .await?;
    let status = resp.status();
    if !status.is_success() {
        let text = resp.text().await.unwrap_or_default();
        return Err(classify_outlook_error(
            &format!("draft.patch {draft_id}"),
            status,
            &text,
        ));
    }
    let send_url = format!("{GRAPH_BASE}/me/messages/{encoded_draft}/send");
    let resp = session
        .client
        .post(&send_url)
        .bearer_auth(token)
        .send()
        .await?;
    let status = resp.status();
    if !status.is_success() {
        let text = resp.text().await.unwrap_or_default();
        return Err(classify_outlook_error(
            &format!("draft.send {draft_id}"),
            status,
            &text,
        ));
    }
    Ok(())
}

/// Result of a draft-cleanup attempt. Distinguishes "draft was gone
/// when we tried to delete" (which strongly suggests the prior send call
/// did succeed even though we saw a network error — roborev 181 #3)
/// from "we actually deleted the draft" and "delete itself failed".
#[derive(Debug, PartialEq, Eq)]
enum DraftCleanup {
    Deleted,
    AlreadyGone,
    Failed(String),
}

/// Map (send_err, cleanup, draft_id) → final error surfaced to the
/// caller. Pure so the `AlreadyGone` "check Sent Items" wording is
/// pinned by unit tests (roborev 181 #3).
fn format_send_failure_with_cleanup(
    send_err: Error,
    cleanup: DraftCleanup,
    draft_id: &str,
) -> Error {
    match cleanup {
        DraftCleanup::Deleted => send_err,
        DraftCleanup::AlreadyGone => Error::Internal(format!(
            "send appeared to fail ({send_err}), but the draft was already \
             gone when we tried to clean it up — the message may have \
             actually been sent. Check your Outlook Sent Items folder \
             BEFORE resending to avoid a duplicate."
        )),
        DraftCleanup::Failed(cleanup_err) => Error::Internal(format!(
            "send failed ({send_err}); orphan draft cleanup also failed \
             ({cleanup_err}); draft remains in Drafts folder as {draft_id}"
        )),
    }
}

async fn delete_draft(session: &OutlookSession, token: &str, draft_id: &str) -> DraftCleanup {
    let encoded = crate::provider_utils::encode_path_segment(draft_id);
    let url = format!("{GRAPH_BASE}/me/messages/{encoded}");
    let resp = match session.client.delete(&url).bearer_auth(token).send().await {
        Ok(r) => r,
        Err(e) => return DraftCleanup::Failed(e.to_string()),
    };
    let status = resp.status();
    if status.is_success() {
        DraftCleanup::Deleted
    } else if status.as_u16() == 404 {
        // The draft is gone. Two possibilities:
        // 1. The prior send actually completed despite our network error,
        //    so Graph consumed the draft and moved it to Sent Items.
        // 2. Concurrent UI deleted the draft. Rare; the user-facing
        //    message handles both by mentioning Sent Items.
        DraftCleanup::AlreadyGone
    } else {
        DraftCleanup::Failed(format!("delete returned {status}"))
    }
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
            upload_cache: tokio::sync::Mutex::new(HashMap::new()),
            identity_cache: tokio::sync::Mutex::new(None),
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
            upload_cache: tokio::sync::Mutex::new(HashMap::new()),
            identity_cache: tokio::sync::Mutex::new(None),
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
    fn odata_translator_subject_with_spaces_wraps_value_in_kql_quotes() {
        // Roborev 179 #6: KQL parses `subject:meeting tomorrow` as
        // `subject:meeting AND tomorrow`. Multi-word subject values must
        // be wrapped in KQL quotes: subject:"meeting tomorrow".
        let q = ParsedQuery {
            subject: vec!["meeting tomorrow".into()],
            ..Default::default()
        };
        let r = translate_query_to_odata(&q);
        let s = r.search.unwrap();
        // The outer $search wrap escapes the inner KQL quotes.
        // Inside the outer "…", we want subject:\"meeting tomorrow\".
        assert!(s.contains(r#"subject:\"meeting tomorrow\""#), "got {s}");
    }

    #[test]
    fn odata_translator_subject_single_word_no_quoting() {
        // Single-word subject doesn't need quoting — keep the simpler shape.
        let q = ParsedQuery {
            subject: vec!["urgent".into()],
            ..Default::default()
        };
        let r = translate_query_to_odata(&q);
        let s = r.search.unwrap();
        assert!(s.contains("subject:urgent"));
        // No backslash-quote sequence around "urgent" since it's unquoted.
        assert!(!s.contains(r#"subject:\"urgent\""#));
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

    // ---- Roborev 179 #10: parse_graph_attachments validates IDs ----
    //
    // If Graph ever returns an `id` containing `:`, the serialized blob_id
    // (`outlook:msg:att`) would round-trip into the wrong msg/att split.
    // Defensive: skip such attachments at construction with a warn.

    #[test]
    fn parse_graph_attachments_skips_msg_id_with_colon() {
        // Construct with a msg_id that itself contains ':' — must be
        // filtered out so the blob_id doesn't deserialize to garbage.
        let attachments_json = serde_json::json!([
            {
                "id": "valid-att-1",
                "name": "ok.pdf",
                "contentType": "application/pdf",
                "size": 100
            }
        ]);
        let parsed = parse_graph_attachments("bad:msg:id", &attachments_json);
        // The bad msg_id means we can't safely construct any blob_id —
        // expect empty (defensive skip).
        assert!(parsed.is_empty(), "got {parsed:?}");
    }

    #[test]
    fn parse_graph_attachments_skips_att_id_with_colon() {
        let attachments_json = serde_json::json!([
            {
                "id": "evil:att:id",
                "name": "weird.bin",
                "contentType": "application/octet-stream",
                "size": 1
            },
            {
                "id": "good-att-id",
                "name": "ok.bin",
                "contentType": "application/octet-stream",
                "size": 1
            }
        ]);
        let parsed = parse_graph_attachments("safe-msg-id", &attachments_json);
        // Only the well-formed one survives.
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].name, "ok.bin");
    }

    #[test]
    fn parse_graph_attachments_accepts_normal_graph_ids() {
        // Real Graph IDs are long base64; they don't contain ':'. Sanity
        // that the validation doesn't reject the common case.
        let attachments_json = serde_json::json!([
            {
                "id": "AAMkADA1ZTI5MDdkLTczODItNDhmZi1iMzM2LTUyMjdiMzNkNTM0OQBGAAAAAAA=",
                "name": "report.pdf",
                "contentType": "application/pdf",
                "size": 12345
            }
        ]);
        let parsed = parse_graph_attachments("msg-abc", &attachments_json);
        assert_eq!(parsed.len(), 1);
        assert!(parsed[0].blob_id.starts_with("outlook:msg-abc:"));
    }

    // ---- Roborev 179 #1 + #11: build_outlook_query_url $orderby/$search ----

    #[test]
    fn url_builder_includes_orderby_when_no_search() {
        let odata = OdataQuery {
            filter: Some("isRead eq false".into()),
            search: None,
        };
        let url = build_outlook_query_url(None, &odata, 25, 0);
        assert!(
            url.contains("%24orderby=receivedDateTime+desc")
                || url.contains("%24orderby=receivedDateTime%20desc"),
            "expected $orderby in URL when no $search: {url}"
        );
    }

    #[test]
    fn url_builder_omits_orderby_when_search_present() {
        // Roborev 179 #1: Graph rejects $orderby combined with $search
        // (search results are relevance-ranked). Sending both yields HTTP
        // 400 on every search query. Skip $orderby when $search is set.
        let odata = OdataQuery {
            filter: None,
            search: Some(r#""newsletter""#.into()),
        };
        let url = build_outlook_query_url(None, &odata, 25, 0);
        assert!(
            !url.contains("orderby"),
            "expected NO $orderby when $search present: {url}"
        );
    }

    #[test]
    fn url_builder_omits_orderby_when_filter_and_search_both_present() {
        let odata = OdataQuery {
            filter: Some("isRead eq false".into()),
            search: Some(r#""urgent""#.into()),
        };
        let url = build_outlook_query_url(None, &odata, 25, 0);
        assert!(!url.contains("orderby"));
        assert!(url.contains("%24filter=") || url.contains("$filter="));
        assert!(url.contains("%24search=") || url.contains("$search="));
    }

    #[test]
    fn url_builder_uses_folder_path_when_id_given() {
        let url = build_outlook_query_url(Some("inbox"), &OdataQuery::default(), 25, 0);
        assert!(url.contains("/me/mailFolders/inbox/messages"));
    }

    #[test]
    fn url_builder_uses_me_messages_when_folder_none() {
        let url = build_outlook_query_url(None, &OdataQuery::default(), 25, 0);
        assert!(url.contains("/me/messages"));
        assert!(!url.contains("mailFolders"));
    }

    #[test]
    fn url_builder_includes_top_and_skip() {
        let url = build_outlook_query_url(None, &OdataQuery::default(), 50, 100);
        assert!(url.contains("%24top=50") || url.contains("$top=50"));
        assert!(url.contains("%24skip=100") || url.contains("$skip=100"));
    }

    #[test]
    fn url_builder_omits_skip_when_position_zero() {
        let url = build_outlook_query_url(None, &OdataQuery::default(), 25, 0);
        assert!(!url.contains("skip="));
    }

    // ---- Roborev 179 #11: classify_outlook_error tests ----

    #[test]
    fn classify_outlook_4xx_returns_bad_request() {
        let err = classify_outlook_error(
            "messages.list",
            reqwest::StatusCode::BAD_REQUEST,
            "bad query",
        );
        assert!(matches!(err, Error::BadRequest(_)));
    }

    #[test]
    fn classify_outlook_401_returns_bad_request() {
        let err = classify_outlook_error("me.get", reqwest::StatusCode::UNAUTHORIZED, "bad token");
        assert!(matches!(err, Error::BadRequest(_)));
    }

    #[test]
    fn classify_outlook_404_returns_bad_request() {
        let err =
            classify_outlook_error("messages.get xyz", reqwest::StatusCode::NOT_FOUND, "gone");
        assert!(matches!(err, Error::BadRequest(_)));
    }

    #[test]
    fn classify_outlook_5xx_returns_internal() {
        let err = classify_outlook_error(
            "messages.list",
            reqwest::StatusCode::INTERNAL_SERVER_ERROR,
            "down",
        );
        assert!(matches!(err, Error::Internal(_)));
    }

    #[test]
    fn classify_outlook_429_returns_bad_request() {
        // Graph's throttling — 4xx, surfaces to caller. Future work could
        // expose Retry-After but it's still BadRequest-shaped today.
        let err = classify_outlook_error(
            "messages.list",
            reqwest::StatusCode::TOO_MANY_REQUESTS,
            "throttled",
        );
        assert!(matches!(err, Error::BadRequest(_)));
    }

    #[test]
    fn classify_outlook_includes_operation_and_status_in_message() {
        let err = classify_outlook_error(
            "messages.send",
            reqwest::StatusCode::BAD_REQUEST,
            "missing field",
        );
        let msg = match err {
            Error::BadRequest(m) => m,
            other => panic!("expected BadRequest, got {other:?}"),
        };
        assert!(msg.contains("messages.send"));
        assert!(msg.contains("400"));
        assert!(msg.contains("missing field"));
    }

    // =========================================================================
    // Phase 4 Milestone B — write actions
    // =========================================================================

    // ---- move_plan_outlook ----
    //
    // Mirrors Gmail's move_plan from src/gmail.rs. Outlook's move semantics
    // differ in detail (folder IDs vs labels, dedicated `/move` endpoint
    // for both archive and trash), but the discipline is the same:
    // reject system folders that need dedicated endpoints, route INBOX to
    // a restore-from-trash flow, vanilla moves go to user folders.

    #[test]
    fn move_plan_outlook_inbox_restores_from_trash_and_junk() {
        // "Move to Inbox" = restore from any non-inbox location. The plan
        // returns the inbox folder ID as the move target; calling code is
        // responsible for issuing /messages/{id}/move with destinationId.
        assert_eq!(
            move_plan_outlook("inbox"),
            MovePlanOutlook::Move {
                destination: "inbox".into()
            }
        );
    }

    #[test]
    fn move_plan_outlook_trash_routes_to_deleteditems() {
        // Outlook's trash is a folder, not a verb — "move to deleteditems"
        // is the correct restoration of standard semantics.
        assert_eq!(
            move_plan_outlook("deleteditems"),
            MovePlanOutlook::Move {
                destination: "deleteditems".into()
            }
        );
    }

    #[test]
    fn move_plan_outlook_archive_routes_to_archive_folder() {
        assert_eq!(
            move_plan_outlook("archive"),
            MovePlanOutlook::Move {
                destination: "archive".into()
            }
        );
    }

    #[test]
    fn move_plan_outlook_user_folder_passes_through() {
        // User-created folder IDs are long base64 strings; the plan
        // accepts them as-is.
        let folder_id = "AAMkADA1ZTI5USERFOLDER";
        assert_eq!(
            move_plan_outlook(folder_id),
            MovePlanOutlook::Move {
                destination: folder_id.into()
            }
        );
    }

    #[test]
    fn move_plan_outlook_rejects_sentitems() {
        // "Move to Sent" doesn't make sense — Sent is set by the send
        // endpoint automatically. Reject so callers can surface a clear
        // user-visible error.
        match move_plan_outlook("sentitems") {
            MovePlanOutlook::Reject(msg) => assert!(msg.contains("Sent")),
            other => panic!("expected Reject for sentitems, got {other:?}"),
        }
    }

    #[test]
    fn move_plan_outlook_rejects_drafts() {
        // Drafts are created via /me/messages POST or createReply,
        // not via /move from another folder.
        assert!(matches!(
            move_plan_outlook("drafts"),
            MovePlanOutlook::Reject(_)
        ));
    }

    #[test]
    fn move_plan_outlook_rejects_junkemail() {
        // "Move to Junk" wants the markAsJunk action, not a plain folder
        // move (which would skip Outlook's junk-filter learning).
        assert!(matches!(
            move_plan_outlook("junkemail"),
            MovePlanOutlook::Reject(_)
        ));
    }

    // ---- mutation body builders ----

    #[test]
    fn mark_read_body_shape() {
        assert_eq!(mark_read_body(true), serde_json::json!({ "isRead": true }));
        assert_eq!(
            mark_read_body(false),
            serde_json::json!({ "isRead": false })
        );
    }

    #[test]
    fn flag_body_flagged() {
        let body = flag_body(true);
        assert_eq!(body["flag"]["flagStatus"], "flagged");
    }

    #[test]
    fn flag_body_not_flagged() {
        let body = flag_body(false);
        assert_eq!(body["flag"]["flagStatus"], "notFlagged");
    }

    #[test]
    fn move_body_shape() {
        let body = move_body("inbox");
        assert_eq!(body, serde_json::json!({ "destinationId": "inbox" }));
    }

    // ---- chunk_batch_requests ----

    #[test]
    fn chunk_batch_empty_returns_no_chunks() {
        let ids: Vec<String> = vec![];
        let chunks = chunk_batch_requests(&ids);
        assert!(chunks.is_empty());
    }

    #[test]
    fn chunk_batch_one_message_returns_one_chunk() {
        let ids = vec!["m1".to_string()];
        let chunks = chunk_batch_requests(&ids);
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].len(), 1);
    }

    #[test]
    fn chunk_batch_exactly_20_returns_one_chunk() {
        // Graph's /$batch cap is 20. Boundary case must stay in 1 chunk.
        let ids: Vec<String> = (0..20).map(|i| format!("m{i}")).collect();
        let chunks = chunk_batch_requests(&ids);
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].len(), 20);
    }

    #[test]
    fn chunk_batch_21_spans_two_chunks() {
        let ids: Vec<String> = (0..21).map(|i| format!("m{i}")).collect();
        let chunks = chunk_batch_requests(&ids);
        assert_eq!(chunks.len(), 2);
        assert_eq!(chunks[0].len(), 20);
        assert_eq!(chunks[1].len(), 1);
    }

    #[test]
    fn chunk_batch_50_spans_three_chunks() {
        let ids: Vec<String> = (0..50).map(|i| format!("m{i}")).collect();
        let chunks = chunk_batch_requests(&ids);
        assert_eq!(chunks.len(), 3);
        assert_eq!(chunks[0].len(), 20);
        assert_eq!(chunks[1].len(), 20);
        assert_eq!(chunks[2].len(), 10);
    }

    // ---- Roborev 180 #4 + #5: parse_batch_response ----
    //
    // Graph's /$batch returns 200 with a `responses[]` array of per-item
    // results. This parser extracts the per-item outcomes so the caller
    // can distinguish "all-succeeded" from "partial-failure" and aggregate
    // ALL failures (not just the first).

    #[test]
    fn parse_batch_response_all_succeed() {
        let body = serde_json::json!({
            "responses": [
                { "id": "1", "status": 200 },
                { "id": "2", "status": 200 },
                { "id": "3", "status": 200 }
            ]
        });
        let r = parse_batch_response(&body, 3).unwrap();
        assert_eq!(r.succeeded, 3);
        assert!(r.errors.is_empty());
    }

    #[test]
    fn parse_batch_response_204_no_content_counts_as_success() {
        // Graph's /move returns 204 in the per-item response.
        let body = serde_json::json!({
            "responses": [
                { "id": "1", "status": 204 }
            ]
        });
        let r = parse_batch_response(&body, 1).unwrap();
        assert_eq!(r.succeeded, 1);
    }

    #[test]
    fn parse_batch_response_partial_failure_collects_all_errors() {
        // Roborev 180 #5: if items 1, 3, and 5 all fail in the same batch,
        // the user should see all of them, not just item 1's.
        let body = serde_json::json!({
            "responses": [
                { "id": "1", "status": 404, "body": { "error": { "message": "gone-1" } } },
                { "id": "2", "status": 200 },
                { "id": "3", "status": 403, "body": { "error": { "message": "denied-3" } } },
                { "id": "4", "status": 200 },
                { "id": "5", "status": 500, "body": { "error": { "message": "boom-5" } } }
            ]
        });
        let r = parse_batch_response(&body, 5).unwrap();
        assert_eq!(r.succeeded, 2);
        assert_eq!(r.errors.len(), 3);
        // Each failure surfaces its status and message.
        let joined = r.errors.join(" | ");
        assert!(joined.contains("404") && joined.contains("gone-1"));
        assert!(joined.contains("403") && joined.contains("denied-3"));
        assert!(joined.contains("500") && joined.contains("boom-5"));
    }

    #[test]
    fn parse_batch_response_missing_responses_array_is_error() {
        // Roborev 180 #4: if Graph returns 200 with no `responses` key
        // (malformed / future API change / gateway interception), don't
        // silently over-count succeeded — fail loudly.
        let body = serde_json::json!({ "value": [] });
        let err = parse_batch_response(&body, 3).unwrap_err();
        assert!(err.contains("responses"));
    }

    #[test]
    fn parse_batch_response_count_mismatch_is_error() {
        // Submitted 3 but Graph responded with 2 — protocol violation.
        let body = serde_json::json!({
            "responses": [
                { "id": "1", "status": 200 },
                { "id": "2", "status": 200 }
            ]
        });
        let err = parse_batch_response(&body, 3).unwrap_err();
        assert!(err.contains("expected 3") || err.contains("count"));
    }

    #[test]
    fn parse_batch_response_404_is_archive_friendly_flag() {
        // Roborev 180 #2: when the per-item failure is 404, the caller
        // (archive_batch) needs to substitute the actionable "create
        // Archive folder" message.
        let body = serde_json::json!({
            "responses": [
                { "id": "1", "status": 404, "body": { "error": { "message": "archive missing" } } }
            ]
        });
        let r = parse_batch_response(&body, 1).unwrap();
        assert!(
            r.any_404,
            "should flag 404 for the archive friendly-message path"
        );
    }

    // ---- Roborev 180 #6: role_rejects_move ----

    #[test]
    fn role_rejects_drafts() {
        assert!(role_rejects_move(Some("drafts")).is_some());
    }

    #[test]
    fn role_rejects_sent() {
        // Note: Mailbox.role for sent is "sent" (not "sentitems" — that's
        // the Graph well-known name; outlook_folder_role normalizes it).
        assert!(role_rejects_move(Some("sent")).is_some());
    }

    #[test]
    fn role_rejects_junk() {
        assert!(role_rejects_move(Some("junk")).is_some());
    }

    #[test]
    fn role_does_not_reject_inbox_archive_trash_user() {
        assert!(role_rejects_move(Some("inbox")).is_none());
        assert!(role_rejects_move(Some("archive")).is_none());
        assert!(role_rejects_move(Some("trash")).is_none());
        assert!(role_rejects_move(None).is_none());
    }

    // =========================================================================
    // Phase 4 Milestone C — send + compose
    // =========================================================================

    // ---- pick_send_path ----
    //
    // Top-5 greats finding: branch on attachments to save 2 RTTs in the
    // no-attachment reply case. Graph's POST /me/messages/{id}/reply is
    // a single call that handles threading; createReply → PATCH → send
    // is only needed when attachments must be inlined via PATCH.

    fn empty_sub() -> EmailSubmission {
        EmailSubmission {
            to: vec!["alice@example.com".into()],
            cc: vec![],
            subject: "S".into(),
            text_body: "B".into(),
            bcc: None,
            html_body: None,
            in_reply_to: None,
            references: None,
            attachments: vec![],
            calendar_ics: None,
        }
    }

    fn fake_attachment() -> crate::types::Attachment {
        crate::types::Attachment {
            blob_id: "synth:00000000-0000-0000-0000-000000000001".into(),
            name: "a.pdf".into(),
            mime_type: "application/pdf".into(),
            size: 1,
        }
    }

    #[test]
    fn pick_send_path_no_reply_no_attachments_is_new_mail() {
        let sub = empty_sub();
        assert_eq!(pick_send_path(&sub), SendPath::NewMail);
    }

    #[test]
    fn pick_send_path_no_reply_with_attachments_is_new_mail() {
        let mut sub = empty_sub();
        sub.attachments = vec![fake_attachment()];
        // No `in_reply_to` → still NewMail; attachments are inlined in
        // the sendMail body, no draft needed.
        assert_eq!(pick_send_path(&sub), SendPath::NewMail);
    }

    #[test]
    fn pick_send_path_reply_html_only_no_attachments_is_one_call() {
        // Roborev 181 #1: the 1-call /reply path treats `comment` as
        // HTML. Only safe when the user composed HTML.
        let mut sub = empty_sub();
        sub.in_reply_to = Some("parent-msg-id".into());
        sub.html_body = Some("<p>hi</p>".into());
        assert_eq!(pick_send_path(&sub), SendPath::ReplyOneShot);
    }

    #[test]
    fn pick_send_path_reply_plain_text_no_attachments_uses_three_call() {
        // Roborev 181 #1 (the bug): a plain-text reply with newlines
        // collapses to one paragraph if sent via /reply's HTML-rendered
        // `comment` field. Route through the 3-call createReply path
        // which lets us PATCH with contentType="text".
        let mut sub = empty_sub();
        sub.in_reply_to = Some("parent-msg-id".into());
        sub.text_body = "line one\nline two\nline three".into();
        sub.html_body = None;
        assert_eq!(pick_send_path(&sub), SendPath::ReplyWithAttachments);
    }

    #[test]
    fn pick_send_path_reply_with_attachments_is_three_call_create_reply() {
        let mut sub = empty_sub();
        sub.in_reply_to = Some("parent-msg-id".into());
        sub.attachments = vec![fake_attachment()];
        assert_eq!(pick_send_path(&sub), SendPath::ReplyWithAttachments);
    }

    #[test]
    fn pick_send_path_reply_with_empty_attachments_vec_but_html_is_one_shot() {
        let mut sub = empty_sub();
        sub.in_reply_to = Some("parent-msg-id".into());
        sub.attachments = vec![];
        sub.html_body = Some("<p>x</p>".into());
        assert_eq!(pick_send_path(&sub), SendPath::ReplyOneShot);
    }

    // ---- build_graph_message ----
    //
    // Pure: constructs the Graph Message JSON resource used by the
    // sendMail / draft PATCH paths. Attachments are inlined as
    // fileAttachment items with base64 contentBytes — Graph has no
    // standalone blob store, so the bytes must be in the message body.

    #[test]
    fn graph_message_text_only_has_required_fields() {
        let sub = empty_sub();
        let msg = build_graph_message_with_from_identity(&sub, &[], None, None);
        assert_eq!(msg["subject"], "S");
        assert_eq!(msg["body"]["contentType"], "text");
        assert_eq!(msg["body"]["content"], "B");
        let to_arr = msg["toRecipients"].as_array().unwrap();
        assert_eq!(to_arr.len(), 1);
        assert_eq!(to_arr[0]["emailAddress"]["address"], "alice@example.com");
    }

    #[test]
    fn graph_message_html_body_uses_html_contenttype() {
        let mut sub = empty_sub();
        sub.html_body = Some("<p>hi</p>".into());
        let msg = build_graph_message_with_from_identity(&sub, &[], None, None);
        // HTML body wins when present (matches the body the user composed).
        assert_eq!(msg["body"]["contentType"], "html");
        assert_eq!(msg["body"]["content"], "<p>hi</p>");
    }

    #[test]
    fn graph_message_includes_cc_recipients() {
        let mut sub = empty_sub();
        sub.cc = vec!["bob@example.com".into()];
        let msg = build_graph_message_with_from_identity(&sub, &[], None, None);
        let cc_arr = msg["ccRecipients"].as_array().unwrap();
        assert_eq!(cc_arr.len(), 1);
        assert_eq!(cc_arr[0]["emailAddress"]["address"], "bob@example.com");
    }

    #[test]
    fn graph_message_includes_bcc_recipients_when_set() {
        let mut sub = empty_sub();
        sub.bcc = Some(vec!["carol@example.com".into()]);
        let msg = build_graph_message_with_from_identity(&sub, &[], None, None);
        let bcc_arr = msg["bccRecipients"].as_array().unwrap();
        assert_eq!(bcc_arr.len(), 1);
        assert_eq!(bcc_arr[0]["emailAddress"]["address"], "carol@example.com");
    }

    #[test]
    fn graph_message_omits_bcc_when_none() {
        let sub = empty_sub();
        let msg = build_graph_message_with_from_identity(&sub, &[], None, None);
        // Graph treats absence and `[]` differently for bcc — absent is
        // cleaner. Don't add an empty bccRecipients key.
        assert!(msg.get("bccRecipients").is_none());
    }

    #[test]
    fn graph_message_inlines_attachments_as_file_attachment() {
        // Resolved blobs (after peek_blob_bytes) come in as a Vec<(name,
        // mime, bytes)>. Builder turns them into Graph fileAttachment
        // entries with base64 contentBytes.
        let sub = empty_sub();
        let resolved = vec![(
            "report.pdf".to_string(),
            "application/pdf".to_string(),
            b"%PDF-fake".to_vec(),
        )];
        let msg = build_graph_message_with_from_identity(&sub, &resolved, None, None);
        let atts = msg["attachments"].as_array().unwrap();
        assert_eq!(atts.len(), 1);
        assert_eq!(atts[0]["@odata.type"], "#microsoft.graph.fileAttachment");
        assert_eq!(atts[0]["name"], "report.pdf");
        assert_eq!(atts[0]["contentType"], "application/pdf");
        // contentBytes is standard base64 of "%PDF-fake"
        use base64::Engine;
        let expected = base64::engine::general_purpose::STANDARD.encode(b"%PDF-fake");
        assert_eq!(atts[0]["contentBytes"], expected);
    }

    #[test]
    fn graph_message_omits_attachments_when_resolved_empty() {
        let sub = empty_sub();
        let msg = build_graph_message_with_from_identity(&sub, &[], None, None);
        assert!(msg.get("attachments").is_none());
    }

    #[test]
    fn graph_message_multiple_to_recipients() {
        let mut sub = empty_sub();
        sub.to = vec!["a@x.com".into(), "b@y.com".into(), "c@z.com".into()];
        let msg = build_graph_message_with_from_identity(&sub, &[], None, None);
        let to_arr = msg["toRecipients"].as_array().unwrap();
        assert_eq!(to_arr.len(), 3);
    }

    // ---- Roborev 181 #6: build_graph_reply_patch_body ----
    //
    // The 3-call reply path uses createReply, which pre-populates the
    // draft with subject ("RE: …"), parent's toRecipients,
    // inReplyTo / references / conversationId, and the threading
    // internetMessageHeaders. PATCHing the draft must NOT clobber those
    // — set only the fields the user actually edits (body, attachments,
    // additional cc/bcc).

    #[test]
    fn reply_patch_body_omits_subject_and_to() {
        let mut sub = empty_sub();
        sub.subject = "Re: original".into();
        sub.to = vec!["whatever@example.com".into()];
        let body = build_graph_reply_patch_body(&sub, &[]);
        assert!(
            body.get("subject").is_none(),
            "subject must NOT be in PATCH — createReply set it"
        );
        assert!(
            body.get("toRecipients").is_none(),
            "toRecipients must NOT be in PATCH — createReply set them"
        );
    }

    #[test]
    fn reply_patch_body_includes_html_body_when_set() {
        let mut sub = empty_sub();
        sub.html_body = Some("<p>html reply</p>".into());
        let body = build_graph_reply_patch_body(&sub, &[]);
        assert_eq!(body["body"]["contentType"], "html");
        assert_eq!(body["body"]["content"], "<p>html reply</p>");
    }

    #[test]
    fn reply_patch_body_uses_text_contenttype_for_plain_text() {
        // Roborev 181 #1 fix verification: PATCH path must set
        // contentType="text" so plain-text replies preserve newlines.
        let mut sub = empty_sub();
        sub.text_body = "line one\nline two".into();
        sub.html_body = None;
        let body = build_graph_reply_patch_body(&sub, &[]);
        assert_eq!(body["body"]["contentType"], "text");
        assert_eq!(body["body"]["content"], "line one\nline two");
    }

    #[test]
    fn reply_patch_body_includes_added_cc_bcc() {
        let mut sub = empty_sub();
        sub.cc = vec!["added-cc@example.com".into()];
        sub.bcc = Some(vec!["added-bcc@example.com".into()]);
        let body = build_graph_reply_patch_body(&sub, &[]);
        assert_eq!(
            body["ccRecipients"][0]["emailAddress"]["address"],
            "added-cc@example.com"
        );
        assert_eq!(
            body["bccRecipients"][0]["emailAddress"]["address"],
            "added-bcc@example.com"
        );
    }

    #[test]
    fn reply_patch_body_includes_attachments() {
        let sub = empty_sub();
        let resolved = vec![(
            "report.pdf".to_string(),
            "application/pdf".to_string(),
            b"%PDF".to_vec(),
        )];
        let body = build_graph_reply_patch_body(&sub, &resolved);
        let atts = body["attachments"].as_array().unwrap();
        assert_eq!(atts.len(), 1);
        assert_eq!(atts[0]["name"], "report.pdf");
    }

    // ---- Roborev 181 #5: build_graph_message with from_addr ----

    #[test]
    fn graph_message_includes_from_when_provided() {
        let sub = empty_sub();
        let msg = build_graph_message_with_from_identity(&sub, &[], Some("shared@org.com"), None);
        assert_eq!(msg["from"]["emailAddress"]["address"], "shared@org.com");
    }

    #[test]
    fn graph_message_omits_from_when_none() {
        let sub = empty_sub();
        let msg = build_graph_message_with_from_identity(&sub, &[], None, None);
        assert!(
            msg.get("from").is_none(),
            "no 'from' field when caller didn't provide one — Graph picks default"
        );
    }

    #[test]
    fn graph_message_omits_from_when_empty_string() {
        let sub = empty_sub();
        let msg = build_graph_message_with_from_identity(&sub, &[], Some(""), None);
        assert!(msg.get("from").is_none());
    }

    // ---- Milestone D.1: from.emailAddress.name on Graph message ----
    //
    // The picker resolves a display name; the builder must thread it
    // onto Graph's typed Message resource as
    // `from.emailAddress.name`. Without this, the picker's output is
    // discarded and the recipient sees a name-less From line for
    // shared-mailbox sends.

    #[test]
    fn graph_message_includes_from_name_when_provided() {
        let sub = empty_sub();
        let msg = build_graph_message_with_from_identity(
            &sub,
            &[],
            Some("shared@org.com"),
            Some("Shared Inbox"),
        );
        assert_eq!(msg["from"]["emailAddress"]["address"], "shared@org.com");
        assert_eq!(msg["from"]["emailAddress"]["name"], "Shared Inbox");
    }

    #[test]
    fn graph_message_omits_from_name_when_none() {
        let sub = empty_sub();
        let msg = build_graph_message_with_from_identity(&sub, &[], Some("shared@org.com"), None);
        assert_eq!(msg["from"]["emailAddress"]["address"], "shared@org.com");
        // No "name" key when caller didn't resolve one — Graph picks
        // the default rendering rather than rendering an empty name.
        assert!(
            msg["from"]["emailAddress"].get("name").is_none(),
            "no 'name' on from when picker returned None: {msg}"
        );
    }

    // ---- Milestone D.1: pick_outlook_display_name ----
    //
    // Pure picker that mirrors Gmail's pick_identity_display_name discipline:
    // when identity_id_override is supplied but does not match, refuse to
    // fall back to a different identity (don't mislabel From). Outlook's
    // identity list is single-element today (just /me), but the helper
    // takes a slice so the contract survives if alias enumeration lands.

    fn id(id_: &str, email: &str, name: &str) -> crate::types::Identity {
        crate::types::Identity {
            id: id_.into(),
            email: email.into(),
            name: name.into(),
        }
    }

    #[test]
    fn picker_returns_none_for_empty_identities() {
        assert_eq!(pick_outlook_display_name(&[], "u@example.com", None), None);
    }

    #[test]
    fn picker_uses_identity_name_when_from_addr_matches() {
        let ids = vec![id("id-1", "u@example.com", "Ursula Example")];
        assert_eq!(
            pick_outlook_display_name(&ids, "u@example.com", None),
            Some("Ursula Example".into())
        );
    }

    #[test]
    fn picker_returns_none_when_from_addr_does_not_match_and_no_override() {
        // Stricter than Gmail: Outlook has exactly one identity (the
        // user's primary) so a non-matching from_addr means the user
        // is trying to send-as something they don't own — attaching the
        // primary identity's name would be mislabeling, not a best
        // effort. Refuse the fallback.
        let ids = vec![id("id-1", "u@example.com", "Ursula Example")];
        assert_eq!(
            pick_outlook_display_name(&ids, "other@example.com", None),
            None
        );
    }

    #[test]
    fn picker_returns_none_when_identity_has_empty_name() {
        let ids = vec![id("id-1", "u@example.com", "")];
        assert_eq!(pick_outlook_display_name(&ids, "u@example.com", None), None);
    }

    #[test]
    fn picker_returns_name_when_override_matches_identity_id() {
        let ids = vec![
            id("id-1", "u@example.com", "Ursula Example"),
            id("id-2", "alias@example.com", "Ursula Alias"),
        ];
        assert_eq!(
            pick_outlook_display_name(&ids, "alias@example.com", Some("id-2")),
            Some("Ursula Alias".into())
        );
    }

    #[test]
    fn picker_returns_none_when_override_id_unknown() {
        // Key discipline test: override supplied but invalid → return None
        // and do NOT fall back to another identity (Gmail line 1744 parity).
        // Caller emits a tracing::warn so the operator notices.
        let ids = vec![id("id-1", "u@example.com", "Ursula Example")];
        assert_eq!(
            pick_outlook_display_name(&ids, "u@example.com", Some("does-not-exist")),
            None
        );
    }

    #[test]
    fn picker_match_is_case_insensitive_on_from_addr() {
        let ids = vec![id("id-1", "u@example.com", "Ursula Example")];
        assert_eq!(
            pick_outlook_display_name(&ids, "U@EXAMPLE.COM", None),
            Some("Ursula Example".into())
        );
    }

    #[test]
    fn picker_returns_none_when_override_id_matches_but_from_addr_differs() {
        // Roborev 183 #1 defense-in-depth: if the frontend hands us a
        // valid override id whose identity email *differs* from
        // from_addr, attaching the identity's name to from_addr would
        // mislabel From — the same failure mode the unknown-id branch
        // already guards against. Refuse the match.
        let ids = vec![
            id("id-1", "u@example.com", "Ursula Example"),
            id("id-2", "alias@example.com", "Ursula Alias"),
        ];
        assert_eq!(
            pick_outlook_display_name(&ids, "u@example.com", Some("id-2")),
            None,
            "must not attach Ursula Alias's name to u@example.com — that would mislabel From"
        );
    }

    #[test]
    fn picker_override_email_match_is_case_insensitive() {
        // Parallel to the no-override case_insensitive test above —
        // a stored identity email of "u@example.com" should match a
        // from_addr of "U@EXAMPLE.COM" when override is in play too.
        let ids = vec![id("id-1", "u@example.com", "Ursula Example")];
        assert_eq!(
            pick_outlook_display_name(&ids, "U@EXAMPLE.COM", Some("id-1")),
            Some("Ursula Example".into())
        );
    }

    // ---- Roborev 183 #5: pick_from_name_for_send_path ----
    //
    // Pure wiring helper that decides whether send_email should invoke
    // the picker for a given (path, from_addr, override) tuple. Lets us
    // pin the "reply paths skip picker", "empty from skips picker",
    // "NewMail+from_addr calls picker" branches without HTTP mocking
    // (which the codebase deliberately doesn't have).

    #[test]
    fn pick_from_name_skips_picker_on_reply_one_shot() {
        let ids = vec![id("id-1", "u@example.com", "Ursula Example")];
        assert_eq!(
            pick_from_name_for_send_path(SendPath::ReplyOneShot, Some("u@example.com"), None, &ids),
            None,
            "reply paths inherit From from parent message Graph-side; picker output is irrelevant"
        );
    }

    #[test]
    fn pick_from_name_skips_picker_on_reply_with_attachments() {
        let ids = vec![id("id-1", "u@example.com", "Ursula Example")];
        assert_eq!(
            pick_from_name_for_send_path(
                SendPath::ReplyWithAttachments,
                Some("u@example.com"),
                None,
                &ids
            ),
            None
        );
    }

    #[test]
    fn pick_from_name_skips_picker_when_from_addr_is_none() {
        let ids = vec![id("id-1", "u@example.com", "Ursula Example")];
        assert_eq!(
            pick_from_name_for_send_path(SendPath::NewMail, None, None, &ids),
            None,
            "no from_addr → Graph defaults to authenticated user; picker has nothing to label"
        );
    }

    #[test]
    fn pick_from_name_invokes_picker_on_new_mail_with_from_addr() {
        let ids = vec![id("id-1", "u@example.com", "Ursula Example")];
        assert_eq!(
            pick_from_name_for_send_path(SendPath::NewMail, Some("u@example.com"), None, &ids),
            Some("Ursula Example".into())
        );
    }

    #[test]
    fn pick_from_name_honors_override_on_new_mail() {
        let ids = vec![
            id("id-1", "u@example.com", "Ursula Example"),
            id("id-2", "alias@example.com", "Ursula Alias"),
        ];
        assert_eq!(
            pick_from_name_for_send_path(
                SendPath::NewMail,
                Some("alias@example.com"),
                Some("id-2"),
                &ids
            ),
            Some("Ursula Alias".into())
        );
    }

    // ---- Roborev 181 #3: format_send_failure_with_cleanup ----
    //
    // Pure helper that maps (send_err, cleanup_outcome, draft_id) to the
    // final user-facing Error. Pinned here so the AlreadyGone path keeps
    // mentioning "Sent Items" — the user must be told to check before
    // resending to avoid a duplicate.

    #[test]
    fn cleanup_deleted_returns_original_send_error_unchanged() {
        let original = Error::Internal("graph 503".into());
        let out = format_send_failure_with_cleanup(original, DraftCleanup::Deleted, "draft-1");
        match out {
            Error::Internal(msg) => assert_eq!(msg, "graph 503"),
            other => panic!("expected unchanged Internal error, got {other:?}"),
        }
    }

    #[test]
    fn cleanup_already_gone_message_warns_about_sent_items() {
        let original = Error::Internal("network reset".into());
        let out = format_send_failure_with_cleanup(original, DraftCleanup::AlreadyGone, "draft-2");
        let msg = match out {
            Error::Internal(m) => m,
            other => panic!("expected Internal error, got {other:?}"),
        };
        // Must reference Sent Items so the user checks before resending —
        // this is the duplicate-send guard the roborev finding called out.
        assert!(
            msg.contains("Sent Items"),
            "AlreadyGone message must mention Sent Items: {msg}"
        );
        assert!(
            msg.to_ascii_lowercase().contains("before resending")
                || msg.to_ascii_lowercase().contains("avoid a duplicate"),
            "AlreadyGone message must warn about duplicate sends: {msg}"
        );
        // Original send error context must survive — otherwise the user
        // loses the actual diagnostic.
        assert!(
            msg.contains("network reset"),
            "AlreadyGone message must preserve the original send error: {msg}"
        );
    }

    #[test]
    fn cleanup_failed_message_names_the_orphan_draft() {
        let original = Error::Internal("graph 500".into());
        let out = format_send_failure_with_cleanup(
            original,
            DraftCleanup::Failed("delete returned 503".into()),
            "AAMkAGI-orphan-id",
        );
        let msg = match out {
            Error::Internal(m) => m,
            other => panic!("expected Internal error, got {other:?}"),
        };
        // Need the draft_id so the user can find / clean it up manually.
        assert!(
            msg.contains("AAMkAGI-orphan-id"),
            "Failed cleanup message must include draft_id: {msg}"
        );
        // Both error contexts must survive.
        assert!(msg.contains("graph 500"));
        assert!(msg.contains("delete returned 503"));
    }

    // ---- upload_blob + peek_blob_bytes + drain_consumed_synthetic_blobs ----
    //
    // Mirror the Gmail tests since the contract is the same:
    // cap-enforced on count + per-blob size + aggregate size,
    // peek is non-consuming, drain only removes Synthetic entries.

    fn make_outlook_test_session() -> OutlookSession {
        OutlookSession {
            client: reqwest::Client::new(),
            token: tokio::sync::Mutex::new(OutlookToken {
                access_token: "tok".into(),
                refresh_token: "rtok".into(),
                token_expiry: Utc::now() + chrono::Duration::hours(1),
            }),
            client_id: "test-client".into(),
            token_path: std::path::PathBuf::from("/tmp/test-outlook-session"),
            email: "u@example.com".into(),
            folder_cache: tokio::sync::Mutex::new(None),
            page_cache: tokio::sync::Mutex::new(HashMap::new()),
            upload_cache: tokio::sync::Mutex::new(HashMap::new()),
            identity_cache: tokio::sync::Mutex::new(None),
        }
    }

    #[tokio::test]
    async fn upload_blob_rejects_oversized_per_blob() {
        let session = make_outlook_test_session();
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
    async fn upload_blob_rejects_after_count_cap() {
        let session = make_outlook_test_session();
        for i in 0..UPLOAD_CACHE_CAP {
            upload_blob(&session, "text/plain", &[i as u8])
                .await
                .unwrap_or_else(|e| panic!("upload {i} should succeed: {e:?}"));
        }
        let err = upload_blob(&session, "text/plain", &[99])
            .await
            .unwrap_err();
        match err {
            Error::BadRequest(msg) => assert!(msg.contains("full")),
            other => panic!("expected BadRequest cap, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn upload_blob_rejects_aggregate_overflow() {
        let session = make_outlook_test_session();
        let chunk = vec![0u8; 20 * 1024 * 1024];
        upload_blob(&session, "application/octet-stream", &chunk)
            .await
            .unwrap();
        upload_blob(&session, "application/octet-stream", &chunk)
            .await
            .unwrap();
        // Two 20 MiB blobs = 40 MiB; third would push past 50 MiB cap.
        let err = upload_blob(&session, "application/octet-stream", &chunk)
            .await
            .unwrap_err();
        match err {
            Error::BadRequest(msg) => assert!(msg.contains("aggregate")),
            other => panic!("expected BadRequest aggregate, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn peek_blob_bytes_does_not_consume_synthetic() {
        let session = make_outlook_test_session();
        let (blob_id, _) = upload_blob(&session, "text/plain", b"hello").await.unwrap();
        let (mime, bytes) = peek_blob_bytes(&session, &blob_id).await.unwrap();
        assert_eq!(mime, "text/plain");
        assert_eq!(bytes, b"hello");
        // Second peek still works — cache wasn't consumed.
        let (mime2, bytes2) = peek_blob_bytes(&session, &blob_id).await.unwrap();
        assert_eq!(mime2, "text/plain");
        assert_eq!(bytes2, b"hello");
    }

    #[tokio::test]
    async fn drain_consumed_synthetic_blobs_removes_only_synthetic_entries() {
        let session = make_outlook_test_session();
        let (synth_id, _) = upload_blob(&session, "text/plain", b"X").await.unwrap();
        let atts = vec![
            crate::types::Attachment {
                blob_id: synth_id.clone(),
                name: "a.txt".into(),
                mime_type: "text/plain".into(),
                size: 1,
            },
            // Non-synthetic (OutlookAttachment) → drain should ignore.
            crate::types::Attachment {
                blob_id: "outlook:msg-x:att-y".into(),
                name: "b.pdf".into(),
                mime_type: "application/pdf".into(),
                size: 0,
            },
        ];
        drain_consumed_synthetic_blobs(&session, &atts).await;
        let err = peek_blob_bytes(&session, &synth_id).await.unwrap_err();
        assert!(matches!(err, Error::BadRequest(_)));
    }

    #[tokio::test]
    async fn peek_blob_bytes_rejects_gmail_blob_ref() {
        // Cross-provider blob_id must be rejected explicitly so the
        // synthetic-blob path can't accidentally fetch a Gmail attachment.
        let session = make_outlook_test_session();
        let err = peek_blob_bytes(&session, "msg-1:att-1").await.unwrap_err();
        assert!(matches!(err, Error::BadRequest(_)));
    }
}
