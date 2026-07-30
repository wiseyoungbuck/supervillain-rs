use crate::calendar;
use crate::error::Error;
use crate::rate_limit::RateLimiter;
use crate::types::ParsedQuery;
use crate::types::*;
use regex::Regex;
use serde::{Deserialize, Deserializer};
use std::collections::HashMap;
use std::sync::LazyLock;

// =============================================================================
// JMAP deserialization types (internal to this module)
// =============================================================================

/// Deserialize a value that may be explicit JSON `null` into `T::default()`.
/// `#[serde(default)]` only supplies a default when the key is absent; this
/// also handles `"field": null` which JMAP allows for address headers, subject,
/// preview, and size (RFC 8621 §4.1.2.3).
fn nullable_default<'de, D, T>(deserializer: D) -> Result<T, D::Error>
where
    D: Deserializer<'de>,
    T: Default + Deserialize<'de>,
{
    Option::<T>::deserialize(deserializer).map(|opt| opt.unwrap_or_default())
}

/// JMAP session discovery response
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct JmapSessionResponse {
    pub api_url: Option<String>,
    pub upload_url: Option<String>,
    pub download_url: Option<String>,
    #[serde(default)]
    pub primary_accounts: HashMap<String, String>,
}

/// Recursive MIME body structure part
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BodyStructurePart {
    #[serde(rename = "type", default)]
    pub mime_type: String,
    #[serde(default)]
    pub blob_id: Option<String>,
    #[serde(default)]
    pub part_id: Option<String>,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub disposition: Option<String>,
    #[serde(default)]
    pub cid: Option<String>,
    #[serde(default, deserialize_with = "nullable_default")]
    pub size: i64,
    #[serde(default, deserialize_with = "nullable_default")]
    pub sub_parts: Vec<BodyStructurePart>,
}

/// Body part reference (for textBody/htmlBody arrays)
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct BodyPartRef {
    #[serde(default)]
    pub part_id: String,
}

/// Body value entry from the bodyValues map
#[derive(Debug, Clone, Deserialize)]
struct BodyValue {
    #[serde(default)]
    pub value: String,
}

/// Raw JMAP Email/get response item. Converted to Email after body processing.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct JmapEmailRaw {
    pub id: String,
    // blobId/threadId are optional in the wire format because `properties_override`
    // (see `get_emails`) lets callers fetch a subset — e.g. split-counts asks for
    // only id/from/to/cc/subject. Without `default` the route 500s for Fastmail.
    #[serde(default)]
    pub blob_id: String,
    #[serde(default)]
    pub thread_id: String,
    #[serde(default)]
    pub mailbox_ids: HashMap<String, bool>,
    #[serde(default)]
    pub keywords: HashMap<String, bool>,
    #[serde(default)]
    pub received_at: Option<String>,
    #[serde(default, deserialize_with = "nullable_default")]
    pub subject: String,
    #[serde(default, deserialize_with = "nullable_default")]
    pub from: Vec<EmailAddress>,
    #[serde(default, deserialize_with = "nullable_default")]
    pub to: Vec<EmailAddress>,
    #[serde(default, deserialize_with = "nullable_default")]
    pub cc: Vec<EmailAddress>,
    #[serde(default, deserialize_with = "nullable_default")]
    pub preview: String,
    #[serde(default)]
    pub has_attachment: bool,
    #[serde(default, deserialize_with = "nullable_default")]
    pub size: i64,
    /// RFC 8621 `inReplyTo`: `String[]|null` of Message-IDs (for drafts saved
    /// by this app, the value the client sent at save time). Needed so a
    /// restored draft keeps its threading (kata wm57 review follow-up).
    #[serde(default)]
    pub in_reply_to: Option<Vec<String>>,
    #[serde(default)]
    pub text_body: Vec<BodyPartRef>,
    #[serde(default)]
    pub html_body: Vec<BodyPartRef>,
    #[serde(default)]
    pub body_values: HashMap<String, BodyValue>,
    pub body_structure: Option<BodyStructurePart>,
}

// =============================================================================
// JMAP Session
// =============================================================================

pub struct JmapSession {
    pub client: reqwest::Client,
    pub username: String,
    /// `Bearer <api-token>` — sent to `api.fastmail.com` for JMAP. API tokens
    /// are JMAP/MCP-only; Fastmail rejects them at the CalDAV endpoint.
    pub auth_header: String,
    /// `Basic <base64(username:app_password)>` — sent to `caldav.fastmail.com`
    /// for CalDAV, which requires an app password (not an API token). Empty
    /// when no app password is configured; the CalDAV functions then return
    /// `Error::CalendarAuthUnconfigured` without issuing any request.
    pub caldav_auth_header: String,
    /// CalDAV base URL. Defaults to `https://caldav.fastmail.com`; a field so
    /// the constant lives in one place (not four inline string literals) and
    /// tests can point it at a loopback recorder.
    pub caldav_base: String,
    /// Resolved default calendar collection URL (absolute, no trailing slash),
    /// discovered once via CalDAV PROPFIND and cached for the session
    /// lifetime (kata wybm). Replaces the hardcoded `/Default/` that
    /// 301→404'd on real Fastmail accounts. Populated lazily on first CalDAV
    /// use by `resolve_calendar_collection` via `OnceCell::get_or_try_init`;
    /// the four CalDAV functions append `/{uid}.ics` to it. `tokio::sync::OnceCell`
    /// (not `std::sync::Mutex`) because it coordinates concurrent first-callers
    /// on the same session — the `get_email` flow spawns fire-and-forget
    /// writers — so only one runs the two-PROPFIND discovery while the rest
    /// await its result (the "one discovery per session" contract, not per
    /// call). A failed init leaves the cell empty (cancellation releases the
    /// init permit, no poisoning), so a transient discovery failure retries
    /// on the next call while a permanent one (no writable calendar) keeps
    /// surfacing. The four `&self` CalDAV functions populate it without `&mut`.
    pub caldav_collection_url: tokio::sync::OnceCell<String>,
    pub api_url: Option<String>,
    pub account_id: Option<String>,
    pub upload_url: Option<String>,
    pub download_url: Option<String>,
    pub mailbox_cache: HashMap<String, Mailbox>,
    pub identity_id: Option<String>,
    pub identities: Option<Vec<Identity>>,
    /// Provider-wide rate limiter combining concurrency cap, steady-state
    /// spacing, and Retry-After-aware retry. Fastmail doesn't publish
    /// hard limits — 4 concurrent at 100ms spacing (≈ 10 RPS) is a
    /// conservative starting point. JMAP method-batching (used in
    /// `send_email`, `archive_batch`) further reduces request count.
    pub limiter: std::sync::Arc<RateLimiter>,
}

impl JmapSession {
    /// Build a session holding both Fastmail auth headers.
    ///
    /// `api_token` → `auth_header` (`Bearer`, for JMAP at `api.fastmail.com`).
    /// `app_password` → `caldav_auth_header` (`Basic`, for CalDAV at
    /// `caldav.fastmail.com`). `app_password = None` leaves
    /// `caldav_auth_header` empty; the CalDAV functions surface
    /// `Error::CalendarAuthUnconfigured` on first use rather than failing at
    /// construction — existing configs without an app password load fine.
    pub fn new(username: &str, api_token: &str, app_password: Option<&str>) -> Self {
        Self {
            client: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(30))
                .build()
                .expect("failed to create HTTP client"),
            username: username.into(),
            auth_header: format!("Bearer {api_token}"),
            caldav_auth_header: basic_auth_header(username, app_password),
            caldav_base: "https://caldav.fastmail.com".into(),
            caldav_collection_url: tokio::sync::OnceCell::new(),
            api_url: None,
            account_id: None,
            upload_url: None,
            download_url: None,
            mailbox_cache: HashMap::new(),
            identity_id: None,
            identities: None,
            limiter: std::sync::Arc::new(RateLimiter::new(
                "jmap",
                4,
                std::time::Duration::from_millis(100),
                3,
            )),
        }
    }

    /// Recompute `auth_header` and `caldav_auth_header` from fresh credentials,
    /// in place on the live session. Used when a Fastmail account is updated
    /// (Settings save) so a rotated api-token / newly-added app password takes
    /// effect without a restart — the JMAP session state (api_url, account_id,
    /// mailbox_cache) is account-level and stays valid across a credential
    /// rotation, so no reconnect is needed. The username is unchanged on this
    /// path (a username change is a different account and rebuilds via
    /// `new`/`connect`), so `self.username` is reused for the Basic header.
    pub fn set_credentials(&mut self, api_token: &str, app_password: Option<&str>) {
        self.auth_header = format!("Bearer {api_token}");
        self.caldav_auth_header = basic_auth_header(&self.username, app_password);
    }
}

/// `Basic <base64(username:app_password)>` for CalDAV, or empty when no app
/// password is configured (the CalDAV functions then return
/// `Error::CalendarAuthUnconfigured` without sending it). Shared by
/// `JmapSession::new` and `set_credentials` so the encoding can't drift.
fn basic_auth_header(username: &str, app_password: Option<&str>) -> String {
    use base64::Engine;
    match app_password.filter(|p| !p.is_empty()) {
        Some(p) => format!(
            "Basic {}",
            base64::engine::general_purpose::STANDARD.encode(format!("{username}:{p}"))
        ),
        None => String::new(),
    }
}

// =============================================================================
// JMAP API functions
// =============================================================================

pub async fn connect(s: &mut JmapSession) -> Result<(), Error> {
    let resp = s
        .client
        .get("https://api.fastmail.com/jmap/session")
        .header("Authorization", &s.auth_header)
        .send()
        .await?;

    match resp.status().as_u16() {
        401 => return Err(Error::Auth("Authentication failed (401)".into())),
        403 => return Err(Error::Auth("Access forbidden (403)".into())),
        200 => {}
        code => return Err(Error::Network(format!("HTTP {code}"))),
    }

    let session: JmapSessionResponse = resp.json().await?;

    s.api_url = session.api_url;
    s.upload_url = session.upload_url;
    s.download_url = session.download_url;

    s.account_id = session
        .primary_accounts
        .get("urn:ietf:params:jmap:mail")
        .cloned();

    debug_assert!(s.api_url.is_some(), "JMAP session must have apiUrl");
    debug_assert!(s.account_id.is_some(), "JMAP session must have accountId");

    tracing::info!("Connected to JMAP as {}", s.username);
    Ok(())
}

async fn jmap_call(
    s: &JmapSession,
    method_calls: Vec<serde_json::Value>,
) -> Result<serde_json::Value, Error> {
    let api_url = s.api_url.as_ref().ok_or(Error::NotConnected)?;

    let payload = serde_json::json!({
        "using": [
            "urn:ietf:params:jmap:core",
            "urn:ietf:params:jmap:mail",
            "urn:ietf:params:jmap:submission"
        ],
        "methodCalls": method_calls
    });

    // Route every JMAP request through the session limiter: one wrap
    // covers ~12 call sites since nearly all JMAP operations bottleneck
    // here.
    let resp = s
        .limiter
        .execute("jmap_call", || async {
            s.client
                .post(api_url)
                .header("Authorization", &s.auth_header)
                .json(&payload)
                .send()
                .await
        })
        .await?;

    if !resp.status().is_success() {
        return Err(Error::Network(format!(
            "JMAP call failed: HTTP {}",
            resp.status()
        )));
    }

    let body: serde_json::Value = resp.json().await?;
    // JMAP can return HTTP 200 with `urn:ietf:params:jmap:error:limit`
    // inside individual method responses. Surface that as a typed
    // rate-limit error so upstream layers (and the user) see 429-shaped
    // behavior rather than a confusing success-with-empty-response.
    if crate::rate_limit::is_jmap_rate_limit_response(&body) {
        return Err(Error::RateLimited { retry_after: None });
    }
    Ok(body)
}

/// Extract and deserialize the `list` array from a JMAP method response.
fn extract_list<T: serde::de::DeserializeOwned>(
    resp: &serde_json::Value,
    index: usize,
    method_name: &str,
) -> Result<Vec<T>, Error> {
    let list = resp["methodResponses"][index][1]
        .get("list")
        .ok_or_else(|| Error::Internal(format!("Invalid {method_name} response: missing list")))?
        .clone();
    serde_json::from_value(list)
        .map_err(|e| Error::Internal(format!("Failed to parse {method_name}: {e}")))
}

/// Filter empty name strings to None in EmailAddress lists.
fn fix_empty_names(addrs: &mut [EmailAddress]) {
    for addr in addrs {
        if addr.name.as_deref() == Some("") {
            addr.name = None;
        }
    }
}

pub async fn get_mailboxes(s: &JmapSession) -> Result<Vec<Mailbox>, Error> {
    let account_id = s.account_id.as_ref().ok_or(Error::NotConnected)?;

    let resp = jmap_call(
        s,
        vec![serde_json::json!([
            "Mailbox/get",
            { "accountId": account_id },
            "0"
        ])],
    )
    .await?;

    extract_list::<Mailbox>(&resp, 0, "Mailbox/get")
}

pub async fn get_identities(s: &mut JmapSession) -> Result<Vec<Identity>, Error> {
    if let Some(ref ids) = s.identities {
        return Ok(ids.clone());
    }

    let account_id = s.account_id.as_ref().ok_or(Error::NotConnected)?.clone();

    let resp = jmap_call(
        s,
        vec![serde_json::json!([
            "Identity/get",
            { "accountId": account_id },
            "0"
        ])],
    )
    .await?;

    let identities: Vec<Identity> = extract_list(&resp, 0, "Identity/get")?;

    if s.identity_id.is_none()
        && let Some(first) = identities.first()
    {
        s.identity_id = Some(first.id.clone());
    }

    s.identities = Some(identities.clone());
    Ok(identities)
}

pub async fn get_identity_for_email(
    s: &mut JmapSession,
    email: &str,
) -> Result<Option<String>, Error> {
    let identities = get_identities(s).await?;
    let found = identities
        .iter()
        .find(|i| i.email.eq_ignore_ascii_case(email))
        .map(|i| i.id.clone());
    Ok(found)
}

// =============================================================================
// JMAP filter translation
// =============================================================================

fn to_jmap_filter(query: Option<&ParsedQuery>, mailbox_id: Option<&str>) -> serde_json::Value {
    let mut conditions: Vec<serde_json::Value> = Vec::new();

    if let Some(mb) = mailbox_id {
        conditions.push(serde_json::json!({"inMailbox": mb}));
    }

    if let Some(q) = query {
        for from in &q.from {
            conditions.push(serde_json::json!({"from": from}));
        }
        for to in &q.to {
            conditions.push(serde_json::json!({"to": to}));
        }
        for subject in &q.subject {
            conditions.push(serde_json::json!({"subject": subject}));
        }
        if q.has_attachment {
            conditions.push(serde_json::json!({"hasAttachment": true}));
        }
        if let Some(true) = q.is_unread {
            conditions.push(serde_json::json!({"notKeyword": "$seen"}));
        }
        if let Some(false) = q.is_unread {
            conditions.push(serde_json::json!({"hasKeyword": "$seen"}));
        }
        if let Some(true) = q.is_flagged {
            conditions.push(serde_json::json!({"hasKeyword": "$flagged"}));
        }
        if let Some(after) = q.after {
            conditions.push(serde_json::json!({"after": format!("{}T00:00:00Z", after)}));
        }
        if let Some(before) = q.before {
            conditions.push(serde_json::json!({"before": format!("{}T00:00:00Z", before)}));
        }
        if !q.text.is_empty() {
            conditions.push(serde_json::json!({"text": q.text}));
        }
    }

    match conditions.len() {
        0 => serde_json::json!({}),
        1 => conditions.into_iter().next().unwrap(),
        _ => serde_json::json!({
            "operator": "AND",
            "conditions": conditions
        }),
    }
}

/// Build the JMAP `Email/query` `sort` clause for the given order. Pure —
/// fixture-tested without a JMAP round-trip, same style as `to_jmap_filter`.
fn jmap_sort_clause(sort: EmailSort) -> serde_json::Value {
    let is_ascending = matches!(sort, EmailSort::DateAsc);
    serde_json::json!([{ "property": "receivedAt", "isAscending": is_ascending }])
}

pub async fn query_emails(
    s: &JmapSession,
    mailbox_id: Option<&str>,
    limit: usize,
    position: usize,
    query: Option<&ParsedQuery>,
    sort: EmailSort,
) -> Result<Vec<String>, Error> {
    let account_id = s.account_id.as_ref().ok_or(Error::NotConnected)?;

    let filter = to_jmap_filter(query, mailbox_id);

    let resp = jmap_call(
        s,
        vec![serde_json::json!([
            "Email/query",
            {
                "accountId": account_id,
                "filter": filter,
                "sort": jmap_sort_clause(sort),
                "limit": limit,
                "position": position
            },
            "0"
        ])],
    )
    .await?;

    let ids_value = resp["methodResponses"][0][1]
        .get("ids")
        .ok_or_else(|| Error::Internal("Invalid Email/query response: missing ids".into()))?
        .clone();
    let ids: Vec<String> = serde_json::from_value(ids_value)
        .map_err(|e| Error::Internal(format!("Failed to parse Email/query ids: {e}")))?;

    Ok(ids)
}

pub async fn get_emails(
    s: &JmapSession,
    ids: &[String],
    fetch_body: bool,
    properties_override: Option<&[&str]>,
) -> Result<Vec<Email>, Error> {
    if ids.is_empty() {
        return Ok(vec![]);
    }

    let account_id = s.account_id.as_ref().ok_or(Error::NotConnected)?;

    let mut properties = if let Some(overrides) = properties_override {
        overrides.to_vec()
    } else {
        vec![
            "id",
            "blobId",
            "threadId",
            "mailboxIds",
            "keywords",
            "receivedAt",
            "subject",
            "from",
            "to",
            "cc",
            "preview",
            "hasAttachment",
            "size",
            "inReplyTo",
        ]
    };
    if fetch_body {
        properties.extend_from_slice(&["textBody", "htmlBody", "bodyValues", "bodyStructure"]);
    }

    let mut extra_args = serde_json::Map::new();
    extra_args.insert("accountId".into(), serde_json::json!(account_id));
    extra_args.insert("ids".into(), serde_json::json!(ids));
    extra_args.insert("properties".into(), serde_json::json!(properties));
    extra_args.insert("fetchHTMLBodyValues".into(), serde_json::json!(fetch_body));
    extra_args.insert("fetchTextBodyValues".into(), serde_json::json!(fetch_body));
    extra_args.insert("maxBodyValueBytes".into(), serde_json::json!(1_000_000));
    if fetch_body {
        extra_args.insert(
            "bodyProperties".into(),
            serde_json::json!([
                "partId",
                "blobId",
                "type",
                "name",
                "size",
                "disposition",
                "subParts",
                "cid"
            ]),
        );
    }

    let resp = jmap_call(s, vec![serde_json::json!(["Email/get", extra_args, "0"])]).await?;

    let raw_emails: Vec<JmapEmailRaw> = extract_list(&resp, 0, "Email/get")?;
    let emails = raw_emails
        .into_iter()
        .map(|raw| parse_jmap_email_from_raw(raw, fetch_body))
        .collect();

    Ok(emails)
}

fn parse_jmap_email_from_raw(mut raw: JmapEmailRaw, fetch_body: bool) -> Email {
    fix_empty_names(&mut raw.from);
    fix_empty_names(&mut raw.to);
    fix_empty_names(&mut raw.cc);

    let received_at = raw
        .received_at
        .as_deref()
        .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
        .map(|dt| dt.with_timezone(&chrono::Utc))
        .unwrap_or_else(chrono::Utc::now);

    let mut text_body = None;
    let mut html_body = None;
    let mut has_calendar = false;

    let default_bs = BodyStructurePart::default();
    let body_structure = raw.body_structure.as_ref().unwrap_or(&default_bs);

    if fetch_body {
        // Extract text body from body values
        let parts: Vec<&str> = raw
            .text_body
            .iter()
            .filter_map(|p| raw.body_values.get(&p.part_id).map(|v| v.value.as_str()))
            .collect();
        if !parts.is_empty() {
            text_body = Some(parts.join("\n"));
        }

        // Extract HTML body from body values
        let parts: Vec<&str> = raw
            .html_body
            .iter()
            .filter_map(|p| raw.body_values.get(&p.part_id).map(|v| v.value.as_str()))
            .collect();
        if !parts.is_empty() {
            html_body = Some(parts.join("\n"));
        }

        // Resolve cid: URLs to download URLs for inline images
        if let Some(ref mut html) = html_body
            && html.to_ascii_lowercase().contains("cid:")
        {
            let mut cids = Vec::new();
            collect_inline_cids(body_structure, &mut cids);
            *html = resolve_inline_cids(html, &raw.id, &cids);
        }

        // Check for calendar in body structure
        has_calendar = find_calendar_blob_id(body_structure).is_some();
    }

    let attachments = if fetch_body {
        find_attachments(body_structure)
    } else {
        vec![]
    };

    Email {
        id: raw.id,
        blob_id: raw.blob_id,
        thread_id: raw.thread_id,
        mailbox_ids: raw.mailbox_ids,
        keywords: raw.keywords,
        received_at,
        subject: raw.subject,
        from: raw.from,
        to: raw.to,
        cc: raw.cc,
        preview: raw.preview,
        has_attachment: raw.has_attachment,
        size: raw.size,
        text_body,
        html_body,
        has_calendar,
        attachments,
        // JMAP inReplyTo is a list; a single parent is the only case this app
        // produces (build_draft_email) and all the restore path needs.
        in_reply_to: raw.in_reply_to.and_then(|v| v.into_iter().next()),
    }
}

/// Test-only wrapper: deserializes JSON then delegates to typed parsing.
/// Preserves existing test compatibility while adding deserialization validation.
#[cfg(test)]
fn parse_jmap_email(item: &serde_json::Value, fetch_body: bool) -> Email {
    let raw: JmapEmailRaw = serde_json::from_value(item.clone())
        .unwrap_or_else(|e| panic!("Failed to deserialize JMAP email: {e}"));
    parse_jmap_email_from_raw(raw, fetch_body)
}

pub fn find_attachments(body_structure: &BodyStructurePart) -> Vec<Attachment> {
    let mut attachments = Vec::new();
    collect_attachments(body_structure, false, &mut attachments);
    attachments
}

fn collect_attachments(part: &BodyStructurePart, in_related: bool, out: &mut Vec<Attachment>) {
    let mime_type = &part.mime_type;

    // Recurse into sub-parts for multipart types.
    // JMAP returns "subParts": [] on leaf nodes, so only treat non-empty arrays
    // as multipart containers.  Only direct children of multipart/related get
    // the in_related flag — nested multipart/mixed subtrees reset it.
    if !part.sub_parts.is_empty() {
        let child_in_related = mime_type.eq_ignore_ascii_case("multipart/related");
        for sub in &part.sub_parts {
            collect_attachments(sub, child_in_related, out);
        }
        return;
    }

    // Skip body content types
    if mime_type.eq_ignore_ascii_case("text/plain")
        || mime_type.eq_ignore_ascii_case("text/html")
        || mime_type.eq_ignore_ascii_case("text/calendar")
    {
        return;
    }

    let disposition = part.disposition.as_deref().unwrap_or_default();
    let name = part.name.as_deref().unwrap_or_default();

    // Skip inline parts only inside multipart/related (HTML-embedded images).
    // Gmail marks user-attached photos as disposition=inline in multipart/mixed,
    // so those should still appear as downloadable attachments.
    if disposition.eq_ignore_ascii_case("inline") && in_related {
        return;
    }

    // Include if explicitly marked as attachment, inline (outside related), or has a filename
    if disposition.eq_ignore_ascii_case("attachment")
        || disposition.eq_ignore_ascii_case("inline")
        || !name.is_empty()
    {
        let blob_id = match part.blob_id.as_deref() {
            Some(id) => id.to_string(),
            None => return,
        };

        out.push(Attachment {
            blob_id,
            name: if name.is_empty() {
                "attachment".to_string()
            } else {
                name.to_string()
            },
            mime_type: mime_type.to_ascii_lowercase(),
            size: part.size,
        });
    }
}

/// Percent-encode a string for use as a URL path segment.
fn percent_encode_path(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => {
                out.push('%');
                out.push(char::from(b"0123456789ABCDEF"[(b >> 4) as usize]));
                out.push(char::from(b"0123456789ABCDEF"[(b & 0x0F) as usize]));
            }
        }
    }
    out
}

/// Replace all occurrences of `needle` in `haystack` using case-insensitive matching.
fn replace_case_insensitive(haystack: &str, needle: &str, replacement: &str) -> String {
    let lower_haystack = haystack.to_ascii_lowercase();
    let lower_needle = needle.to_ascii_lowercase();
    let mut result = String::with_capacity(haystack.len());
    let mut start = 0;
    while let Some(pos) = lower_haystack[start..].find(&lower_needle) {
        result.push_str(&haystack[start..start + pos]);
        result.push_str(replacement);
        start += pos + needle.len();
    }
    result.push_str(&haystack[start..]);
    result
}

/// Walk bodyStructure collecting (content_id, blob_id, filename) for inline parts.
fn collect_inline_cids(part: &BodyStructurePart, out: &mut Vec<(String, String, String)>) {
    if !part.sub_parts.is_empty() {
        for sub in &part.sub_parts {
            collect_inline_cids(sub, out);
        }
        return;
    }

    if let Some(cid) = part.cid.as_deref()
        && !cid.is_empty()
        && let Some(blob_id) = part.blob_id.as_deref()
    {
        let name = part.name.as_deref().unwrap_or("inline");
        out.push((cid.to_string(), blob_id.to_string(), name.to_string()));
    }
}

/// Rewrite `cid:` references in `html` to attachment download URLs.
///
/// Replaces longest cid first. `replace_case_insensitive` is a plain
/// substring match, so if a shorter cid (e.g. "1") were replaced before a
/// longer cid that has it as a prefix (e.g. "12"), the short replacement
/// would corrupt the middle of the "cid:12" occurrence — turning
/// `cid:12` into `<url-for-1>2`. Processing longest-first guarantees a
/// longer cid's occurrences are already rewritten (and thus no longer
/// contain the literal text "cid:<shorter-cid>") by the time the shorter
/// cid's replacement runs.
fn resolve_inline_cids(html: &str, email_id: &str, cids: &[(String, String, String)]) -> String {
    let mut ordered: Vec<&(String, String, String)> = cids.iter().collect();
    ordered.sort_by(|a, b| b.0.len().cmp(&a.0.len()));

    let mut result = html.to_string();
    for (cid, blob_id, name) in ordered {
        let encoded_name = percent_encode_path(name);
        let download_url = format!("/api/emails/{email_id}/attachments/{blob_id}/{encoded_name}");
        // HTML-escape the URL for safe injection into src="..." attributes
        let safe_url = download_url
            .replace('&', "&amp;")
            .replace('"', "&quot;")
            .replace('<', "&lt;")
            .replace('>', "&gt;");
        result = replace_case_insensitive(&result, &format!("cid:{cid}"), &safe_url);
    }
    result
}

// =============================================================================
// Blob upload/download
// =============================================================================

pub async fn upload_blob(
    s: &JmapSession,
    content_type: &str,
    body: &[u8],
) -> Result<(String, i64), Error> {
    let account_id = s.account_id.as_ref().ok_or(Error::NotConnected)?;
    let upload_url = s.upload_url.as_ref().ok_or(Error::NotConnected)?;
    let url = upload_url.replace("{accountId}", account_id);

    // Closure-based limiter: body bytes are cloned per attempt so
    // streaming-body retries actually work (the limiter is documented
    // to require a closure precisely for this case).
    let resp = s
        .limiter
        .execute("blob.upload", || async {
            s.client
                .post(&url)
                .header("Authorization", &s.auth_header)
                .header("Content-Type", content_type)
                .body(reqwest::Body::from(body.to_vec()))
                .send()
                .await
        })
        .await?;

    if !resp.status().is_success() {
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        return Err(Error::Internal(format!("Upload failed ({status}): {text}")));
    }

    let result: serde_json::Value = resp.json().await?;
    let blob_id = result["blobId"]
        .as_str()
        .ok_or_else(|| Error::Internal("Missing blobId in upload response".into()))?
        .to_string();
    let size = result["size"].as_i64().unwrap_or(0);
    Ok((blob_id, size))
}

pub async fn download_blob(
    s: &JmapSession,
    blob_id: &str,
    filename: &str,
) -> Result<(String, Vec<u8>), Error> {
    let account_id = s.account_id.as_ref().ok_or(Error::NotConnected)?;
    let download_url = s.download_url.as_ref().ok_or(Error::NotConnected)?;

    let url = download_url
        .replace("{accountId}", account_id)
        .replace("{blobId}", blob_id)
        .replace("{name}", &percent_encode_path(filename))
        .replace("{type}", "application/octet-stream");

    let resp = s
        .limiter
        .execute("blob.download", || async {
            s.client
                .get(&url)
                .header("Authorization", &s.auth_header)
                .send()
                .await
        })
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
    Ok((content_type, bytes.to_vec()))
}

// =============================================================================
// Email actions
// =============================================================================

async fn set_email_keywords(
    s: &JmapSession,
    email_id: &str,
    keywords_patch: serde_json::Value,
) -> Result<bool, Error> {
    debug_assert!(!email_id.is_empty(), "email_id must not be empty");
    let account_id = s.account_id.as_ref().ok_or(Error::NotConnected)?;

    let resp = jmap_call(
        s,
        vec![serde_json::json!([
            "Email/set",
            {
                "accountId": account_id,
                "update": {
                    email_id: keywords_patch
                }
            },
            "0"
        ])],
    )
    .await?;

    let updated = resp["methodResponses"][0][1]["updated"]
        .as_object()
        .is_some_and(|obj| obj.contains_key(email_id));

    Ok(updated)
}

pub async fn mark_read(s: &JmapSession, email_id: &str) -> Result<bool, Error> {
    set_email_keywords(
        s,
        email_id,
        serde_json::json!({
            "keywords/$seen": true
        }),
    )
    .await
}

pub async fn mark_unread(s: &JmapSession, email_id: &str) -> Result<bool, Error> {
    set_email_keywords(
        s,
        email_id,
        serde_json::json!({
            "keywords/$seen": null
        }),
    )
    .await
}

pub async fn toggle_flag(s: &JmapSession, email_id: &str) -> Result<bool, Error> {
    // First get current state
    let emails = get_emails(s, &[email_id.to_string()], false, None).await?;
    let email = emails
        .first()
        .ok_or_else(|| Error::NotFound("Email not found".into()))?;

    if email.is_flagged() {
        set_email_keywords(
            s,
            email_id,
            serde_json::json!({
                "keywords/$flagged": null
            }),
        )
        .await
    } else {
        set_email_keywords(
            s,
            email_id,
            serde_json::json!({
                "keywords/$flagged": true
            }),
        )
        .await
    }
}

pub async fn archive(s: &JmapSession, email_id: &str) -> Result<bool, Error> {
    move_to_role(s, email_id, "archive").await
}

pub async fn trash(s: &JmapSession, email_id: &str) -> Result<bool, Error> {
    move_to_role(s, email_id, "trash").await
}

/// Build a JMAP `Email/set` patch that adds `target_id` to `mailboxIds` and,
/// if `source_id` is given and differs from `target_id`, removes it —
/// using slash-path patches (`"mailboxIds/{id}": true|null`) rather than a
/// bare `{"mailboxIds": {...}}` update. RFC 8621 treats the latter as a full
/// replacement of the `mailboxIds` property, which wipes out membership in
/// any other mailbox the message happens to be in (e.g. a user-created
/// folder alongside the Inbox). The slash-path form only touches the two
/// keys named here, leaving unrelated memberships untouched. Mirrors the
/// pattern already used by `send_email`'s Drafts → Sent transition.
///
/// `source_id` is a best-effort "where this message is probably filed" hint
/// (typically the Inbox) — removing it approximates the intuitive "moved
/// out of X into Y" semantics without requiring a fetch of the message's
/// actual current `mailboxIds`.
fn build_mailbox_move_patch(
    target_id: &str,
    source_id: Option<&str>,
) -> serde_json::Map<String, serde_json::Value> {
    let mut patch = serde_json::Map::new();
    patch.insert(format!("mailboxIds/{target_id}"), serde_json::json!(true));
    if let Some(source_id) = source_id
        && source_id != target_id
    {
        patch.insert(format!("mailboxIds/{source_id}"), serde_json::Value::Null);
    }
    patch
}

/// The Inbox mailbox id, if this account has one — used as the "source" to
/// remove when filing a message elsewhere (archive/trash/move).
fn inbox_mailbox_id(s: &JmapSession) -> Option<String> {
    s.mailbox_cache
        .values()
        .find(|mb| mb.role.as_deref() == Some("inbox"))
        .map(|mb| mb.id.clone())
}

async fn move_to_role(s: &JmapSession, email_id: &str, role: &str) -> Result<bool, Error> {
    debug_assert!(!email_id.is_empty(), "email_id must not be empty");
    let account_id = s.account_id.as_ref().ok_or(Error::NotConnected)?;

    let target_mb = s
        .mailbox_cache
        .values()
        .find(|mb| mb.role.as_deref() == Some(role))
        .ok_or_else(|| Error::Internal(format!("No mailbox with role '{role}'")))?;

    let target_id = target_mb.id.clone();
    let patch = build_mailbox_move_patch(&target_id, inbox_mailbox_id(s).as_deref());

    let resp = jmap_call(
        s,
        vec![serde_json::json!([
            "Email/set",
            {
                "accountId": account_id,
                "update": {
                    email_id: patch
                }
            },
            "0"
        ])],
    )
    .await?;

    let updated = resp["methodResponses"][0][1]["updated"]
        .as_object()
        .is_some_and(|obj| obj.contains_key(email_id));

    Ok(updated)
}

pub async fn move_to_mailbox(
    s: &JmapSession,
    email_id: &str,
    mailbox_id: &str,
) -> Result<bool, Error> {
    debug_assert!(!email_id.is_empty(), "email_id must not be empty");
    debug_assert!(!mailbox_id.is_empty(), "mailbox_id must not be empty");
    let account_id = s.account_id.as_ref().ok_or(Error::NotConnected)?;

    let patch = build_mailbox_move_patch(mailbox_id, inbox_mailbox_id(s).as_deref());

    let resp = jmap_call(
        s,
        vec![serde_json::json!([
            "Email/set",
            {
                "accountId": account_id,
                "update": {
                    email_id: patch
                }
            },
            "0"
        ])],
    )
    .await?;

    let updated = resp["methodResponses"][0][1]["updated"]
        .as_object()
        .is_some_and(|obj| obj.contains_key(email_id));

    Ok(updated)
}

pub async fn archive_batch(s: &JmapSession, email_ids: &[String]) -> Result<usize, Error> {
    if email_ids.is_empty() {
        return Ok(0);
    }
    let account_id = s.account_id.as_ref().ok_or(Error::NotConnected)?;

    let archive_mb = s
        .mailbox_cache
        .values()
        .find(|mb| mb.role.as_deref() == Some("archive"))
        .ok_or_else(|| Error::Internal("No archive mailbox".into()))?;
    let archive_id = archive_mb.id.clone();
    let inbox_id = inbox_mailbox_id(s);

    let mut updates = serde_json::Map::new();
    for id in email_ids {
        updates.insert(
            id.clone(),
            serde_json::Value::Object(build_mailbox_move_patch(&archive_id, inbox_id.as_deref())),
        );
    }

    let resp = jmap_call(
        s,
        vec![serde_json::json!([
            "Email/set",
            {
                "accountId": account_id,
                "update": updates
            },
            "0"
        ])],
    )
    .await?;

    let count = resp["methodResponses"][0][1]["updated"]
        .as_object()
        .map(|obj| obj.len())
        .unwrap_or(0);

    Ok(count)
}

// =============================================================================
// Send email
// =============================================================================

/// Header-injection guard for values interpolated into the client-built
/// RFC 5322 message (`build_itip_mime`): the subject embeds an event
/// summary parsed from third-party ICS, so CR/LF must not reach a header
/// line.
fn strip_crlf(s: &str) -> String {
    s.replace(['\r', '\n'], "")
}

/// base64 with CRLF line breaks every 76 chars (RFC 2045 §6.8 line limit).
fn base64_mime_lines(data: &[u8]) -> String {
    use base64::Engine;
    let b64 = base64::engine::general_purpose::STANDARD.encode(data);
    b64.as_bytes()
        .chunks(76)
        .map(|c| std::str::from_utf8(c).expect("base64 output is ASCII"))
        .collect::<Vec<_>>()
        .join("\r\n")
}

/// RFC 2047 B-encode a Subject that contains non-ASCII (event summaries are
/// user text — "Réunion d'équipe"); short ASCII subjects pass through bare.
/// Long ASCII subjects also take the encoded-word path — the chunking
/// doubles as line folding, keeping `Subject: ` + value within RFC 5322's
/// 78-char SHOULD limit (roborev 416 #3). Chunking at ≤42 UTF-8 bytes (on
/// char boundaries) makes each encoded-word 68 chars — within RFC 2047's
/// 75-char word limit, and short enough that the first word plus the
/// `Subject: ` prefix stays ≤78; continuation words fold onto their own
/// header lines.
fn encode_subject(raw: &str) -> String {
    let s = strip_crlf(raw);
    if s.is_ascii() && s.len() + "Subject: ".len() <= 78 {
        return s;
    }
    fn encoded_word(chunk: &str) -> String {
        use base64::Engine;
        format!(
            "=?UTF-8?B?{}?=",
            base64::engine::general_purpose::STANDARD.encode(chunk.as_bytes())
        )
    }
    let mut words: Vec<String> = Vec::new();
    let mut chunk = String::new();
    for ch in s.chars() {
        if chunk.len() + ch.len_utf8() > 42 {
            words.push(encoded_word(&chunk));
            chunk.clear();
        }
        chunk.push(ch);
    }
    if !chunk.is_empty() {
        words.push(encoded_word(&chunk));
    }
    words.join("\r\n ")
}

/// Multipart boundary for the client-built iTIP message. A fixed value is
/// safe: every part is base64-encoded and no line of base64 output can
/// start with `--`, so the delimiter cannot collide with content.
const ITIP_BOUNDARY: &str = "supervillain-itip";

/// The iTIP method token from the ICS `METHOD:` property (RFC 5545 §3.7.2):
/// `REQUEST` for invites, `REPLY` for RSVPs. It becomes the MIME
/// `Content-Type: text/calendar; method=<token>` parameter that tells the
/// recipient's calendar client how to process the object (RFC 5546 §3.2).
/// `lines()` leaves a trailing `\r` on CRLF input, hence the trim.
fn ics_method(ics: &str) -> Option<String> {
    ics.lines().find_map(|l| {
        let l = l.trim();
        let (key, val) = l.split_once(':')?;
        if key.eq_ignore_ascii_case("METHOD") {
            Some(val.trim().to_string())
        } else {
            None
        }
    })
}

/// Build the complete RFC 5322 message for an iTIP object (REQUEST or REPLY).
///
/// Client-built MIME, not Email/set bodyStructure, because Email/set has no
/// channel for MIME Content-Type *parameters* (kata vt0m, verified against
/// live Fastmail 2026-07-29; kata 2xh9 extended the same constraint from
/// REPLY to REQUEST): `type` must be a bare token, `charset` and
/// `header:Content-Type` on a part are rejected as `invalidProperties`
/// (the two production failures), and a blob's upload Content-Type is
/// silently replaced by one generated from the part's properties. The
/// `method=<token>` parameter is what makes the message an iTIP object the
/// recipient's client will process (RFC 5546 §3.2), and charset=utf-8 is
/// required for non-ASCII ICS (RFC 5545 §8.1) — so the whole message is
/// built here and delivered via Email/import, which Fastmail preserves
/// byte-for-byte. Date and Message-Id are omitted: Fastmail's MTA adds
/// both at submission (verified live).
///
/// `attachment_bytes` is parallel to `sub.attachments` — the pure builder
/// takes the bytes as input so it stays network-free and testable; the
/// caller (`send_itip_via_import`) downloads each blob. Cc rides on a `Cc:`
/// header; Bcc is envelope-only and never appears in the headers.
fn build_itip_mime(sub: &EmailSubmission, from_addr: &str, attachment_bytes: &[Vec<u8>]) -> String {
    let ics = sub
        .calendar_ics
        .as_deref()
        .expect("build_itip_mime requires calendar_ics");
    let method = ics_method(ics)
        .expect("build_itip_mime: ICS must contain a METHOD line (REQUEST or REPLY)");
    let to = sub
        .to
        .iter()
        .map(|a| strip_crlf(a))
        .collect::<Vec<_>>()
        .join(", ");
    let b = ITIP_BOUNDARY;
    let mut headers = format!("From: {from}\r\nTo: {to}\r\n", from = strip_crlf(from_addr));
    if !sub.cc.is_empty() {
        let cc = sub
            .cc
            .iter()
            .map(|a| strip_crlf(a))
            .collect::<Vec<_>>()
            .join(", ");
        headers.push_str(&format!("Cc: {cc}\r\n"));
    }
    headers.push_str(&format!(
        "Subject: {subject}\r\n\
         MIME-Version: 1.0\r\n\
         Content-Type: multipart/mixed; boundary=\"{b}\"\r\n\
         \r\n",
        subject = encode_subject(&sub.subject),
    ));
    let mut body = String::new();
    body.push_str(&format!(
        "--{b}\r\n\
         Content-Type: text/plain; charset=utf-8\r\n\
         Content-Transfer-Encoding: base64\r\n\
         \r\n\
         {text}\r\n",
        text = base64_mime_lines(sub.text_body.as_bytes()),
    ));
    body.push_str(&format!(
        "--{b}\r\n\
         Content-Type: text/calendar; method={method}; charset=utf-8\r\n\
         Content-Transfer-Encoding: base64\r\n\
         \r\n\
         {cal}\r\n",
        cal = base64_mime_lines(ics.as_bytes()),
    ));
    for (att, bytes) in sub.attachments.iter().zip(attachment_bytes) {
        let mt = if att.mime_type.is_empty() {
            "application/octet-stream"
        } else {
            att.mime_type.as_str()
        };
        // strip_crlf guards against header injection; dropping `"`/`\` keeps
        // the quoted-string parameter intact (filenames rarely carry them).
        let name = strip_crlf(&att.name).replace(['"', '\\'], "");
        body.push_str(&format!(
            "--{b}\r\n\
             Content-Type: {mt}; name=\"{name}\"\r\n\
             Content-Transfer-Encoding: base64\r\n\
             Content-Disposition: attachment; filename=\"{name}\"\r\n\
             \r\n\
             {data}\r\n",
            data = base64_mime_lines(bytes),
        ));
    }
    body.push_str(&format!("--{b}--\r\n"));
    format!("{headers}{body}")
}

/// Human-readable detail for a failed create in a `/set`-style method
/// response: prefer `notCreated`, but fall back to the whole method-response
/// argument — a method-level failure is `["error", {"type": …}, id]`, where
/// `notCreated` doesn't exist and the argument IS the error object
/// (roborev 416 #4).
fn create_failure_detail(resp: &serde_json::Value, index: usize) -> String {
    let args = &resp["methodResponses"][index][1];
    let not_created = &args["notCreated"];
    if not_created.is_null() {
        args.to_string()
    } else {
        not_created.to_string()
    }
}

/// SMTP envelope recipients for a submission: to + cc + bcc.
fn envelope_rcpt_to(sub: &EmailSubmission) -> Vec<serde_json::Value> {
    let mut rcpt_to: Vec<serde_json::Value> = sub
        .to
        .iter()
        .map(|e| serde_json::json!({"email": e}))
        .collect();
    rcpt_to.extend(sub.cc.iter().map(|e| serde_json::json!({"email": e})));
    if let Some(ref bcc) = sub.bcc {
        rcpt_to.extend(bcc.iter().map(|e| serde_json::json!({"email": e})));
    }
    rcpt_to
}

/// Send an iTIP object (REQUEST invite or REPLY RSVP) as a client-built
/// RFC822 message: download any attachment blobs, upload the whole message
/// as `message/rfc822`, then one JMAP request — Email/import into Drafts
/// (`$draft`) plus EmailSubmission/set referencing the import through the
/// `#e` creation id, moving the message Drafts→Sent on success exactly like
/// the plain send path. See `build_itip_mime` for why this bypasses
/// Email/set bodyStructure entirely (Email/set cannot carry the MIME
/// `method` parameter — kata vt0m for REPLY, kata 2xh9 for REQUEST).
async fn send_itip_via_import(
    s: &JmapSession,
    sub: &EmailSubmission,
    from_addr: &str,
    account_id: &str,
    identity_id: &str,
    drafts_id: &str,
    sent_id: &str,
) -> Result<Option<String>, Error> {
    // The client-built MIME carries text + calendar (+ Cc header + inlined
    // attachments) and nothing else, so fields the builder does not emit are
    // rejected rather than silently dropped (roborev 416 #1, 420 #1: every
    // EmailSubmission field the builder ignores is checked). Neither iTIP
    // producer — rsvp() REPLY or send_invite_handler REQUEST — sets these.
    if sub.html_body.is_some() || sub.in_reply_to.is_some() || sub.references.is_some() {
        return Err(Error::Internal(
            "send_itip_via_import does not support html_body, in_reply_to, or references".into(),
        ));
    }
    // Attachments are server-side blobs (uploaded via /api/upload).
    // Email/import takes a single complete RFC822 message, so each attachment
    // is downloaded and inlined as a base64 part rather than referenced by
    // blobId (the way the Email/set path does for non-calendar mail).
    let mut attachment_bytes: Vec<Vec<u8>> = Vec::with_capacity(sub.attachments.len());
    for att in &sub.attachments {
        let (_, bytes) = download_blob(s, &att.blob_id, &att.name).await?;
        attachment_bytes.push(bytes);
    }
    let mime = build_itip_mime(sub, from_addr, &attachment_bytes);
    let (blob_id, _) = upload_blob(s, "message/rfc822", mime.as_bytes()).await?;

    let mut patch = serde_json::Map::new();
    patch.insert(format!("mailboxIds/{drafts_id}"), serde_json::Value::Null);
    patch.insert(format!("mailboxIds/{sent_id}"), serde_json::json!(true));
    patch.insert("keywords/$draft".into(), serde_json::Value::Null);

    let resp = jmap_call(
        s,
        vec![
            serde_json::json!([
                "Email/import",
                {
                    "accountId": account_id,
                    "emails": {
                        "e": {
                            "blobId": blob_id,
                            "mailboxIds": { drafts_id: true },
                            "keywords": { "$draft": true }
                        }
                    }
                },
                "0"
            ]),
            serde_json::json!([
                "EmailSubmission/set",
                {
                    "accountId": account_id,
                    "create": {
                        "send": {
                            "emailId": "#e",
                            "identityId": identity_id,
                            "envelope": {
                                "mailFrom": { "email": from_addr },
                                "rcptTo": envelope_rcpt_to(sub)
                            }
                        }
                    },
                    "onSuccessUpdateEmail": { "#send": patch }
                },
                "1"
            ]),
        ],
    )
    .await?;

    let imported = &resp["methodResponses"][0][1]["created"]["e"];
    if imported.is_null() {
        return Err(Error::Internal(format!(
            "Email import failed: {}",
            create_failure_detail(&resp, 0)
        )));
    }

    let submission = &resp["methodResponses"][1][1]["created"]["send"];
    if submission.is_null() {
        return Err(Error::Internal(format!(
            "Email submission failed: {}",
            create_failure_detail(&resp, 1)
        )));
    }

    Ok(submission["emailId"]
        .as_str()
        .or_else(|| imported["id"].as_str())
        .map(String::from))
}

fn build_draft_email(
    sub: &EmailSubmission,
    from_addr: &str,
    drafts_mailbox_id: &str,
) -> serde_json::Map<String, serde_json::Value> {
    let mut m: serde_json::Map<String, serde_json::Value> = serde_json::Map::new();
    m.insert(
        "mailboxIds".into(),
        serde_json::json!({ drafts_mailbox_id: true }),
    );
    m.insert("from".into(), serde_json::json!([{"email": from_addr}]));
    m.insert(
        "to".into(),
        serde_json::json!(
            sub.to
                .iter()
                .map(|e| serde_json::json!({"email": e}))
                .collect::<Vec<_>>()
        ),
    );
    m.insert("subject".into(), serde_json::json!(sub.subject));

    // JMAP RFC 8621: when bodyStructure is given, textBody/htmlBody MUST NOT
    // appear at the top level.  We always set bodyStructure, so content is
    // defined entirely through bodyStructure + bodyValues with partId refs.

    // Release-enforced (kata 2xh9): calendar_ics (iTIP REQUEST/REPLY) routes
    // through send_itip_via_import (Email/import) — Email/set cannot carry
    // the MIME `method` parameter, so a calendar message built here would go
    // out as a plain calendar attachment, mislabeled. Reaching this builder
    // with calendar_ics is a routing bug — fail loudly, never send it.
    assert!(
        sub.calendar_ics.is_none(),
        "calendar_ics routes through send_itip_via_import, not build_draft_email"
    );
    if let Some(ref html) = sub.html_body {
        m.insert(
            "bodyValues".into(),
            serde_json::json!({
                "body": { "value": sub.text_body },
                "html": { "value": html }
            }),
        );
        m.insert(
            "bodyStructure".into(),
            serde_json::json!({
                "type": "multipart/alternative",
                "subParts": [
                    { "partId": "body", "type": "text/plain" },
                    { "partId": "html", "type": "text/html" }
                ]
            }),
        );
    } else {
        m.insert(
            "bodyValues".into(),
            serde_json::json!({
                "body": { "value": sub.text_body }
            }),
        );
        m.insert(
            "bodyStructure".into(),
            serde_json::json!({
                "type": "text/plain",
                "partId": "body"
            }),
        );
    }

    // Stage 2: wrap with attachments if present
    if !sub.attachments.is_empty() {
        let attachment_parts: Vec<serde_json::Value> = sub
            .attachments
            .iter()
            .map(|a| {
                serde_json::json!({
                    "type": a.mime_type,
                    "blobId": a.blob_id,
                    "name": a.name,
                    "disposition": "attachment",
                    "size": a.size
                })
            })
            .collect();

        // With calendar_ics routed through the import path, bodyStructure is
        // text/plain or multipart/alternative — wrap it in a new
        // multipart/mixed with the attachment parts as siblings.
        let mut sub_parts = vec![m.remove("bodyStructure").unwrap()];
        sub_parts.extend(attachment_parts);
        m.insert(
            "bodyStructure".into(),
            serde_json::json!({
                "type": "multipart/mixed",
                "subParts": sub_parts
            }),
        );
    }

    if !sub.cc.is_empty() {
        m.insert(
            "cc".into(),
            serde_json::json!(
                sub.cc
                    .iter()
                    .map(|e| serde_json::json!({"email": e}))
                    .collect::<Vec<_>>()
            ),
        );
    }

    if let Some(ref bcc) = sub.bcc
        && !bcc.is_empty()
    {
        m.insert(
            "bcc".into(),
            serde_json::json!(
                bcc.iter()
                    .map(|e| serde_json::json!({"email": e}))
                    .collect::<Vec<_>>()
            ),
        );
    }

    if let Some(ref reply_to) = sub.in_reply_to {
        m.insert("inReplyTo".into(), serde_json::json!([reply_to]));
    }

    if let Some(ref refs) = sub.references {
        m.insert("references".into(), serde_json::json!(refs));
    }

    m
}

pub async fn send_email(
    s: &mut JmapSession,
    sub: &EmailSubmission,
    from_addr: &str,
    identity_id_override: Option<&str>,
) -> Result<Option<String>, Error> {
    let account_id = s.account_id.as_ref().ok_or(Error::NotConnected)?.clone();

    // Resolve identity
    let identity_id = if let Some(id) = identity_id_override {
        id.to_string()
    } else if from_addr != s.username {
        match get_identity_for_email(s, from_addr).await? {
            Some(id) => id,
            None => match &s.identity_id {
                Some(id) => id.clone(),
                None => {
                    return Err(Error::Internal(format!(
                        "No identity found for {from_addr}"
                    )));
                }
            },
        }
    } else {
        match &s.identity_id {
            Some(id) => id.clone(),
            None => {
                // Try fetching identities
                get_identities(s).await?;
                match &s.identity_id {
                    Some(id) => id.clone(),
                    None => return Err(Error::Internal("No identities configured".into())),
                }
            }
        }
    };

    // JMAP requires mailboxIds — put the draft in Drafts, move to Sent on success
    let drafts_id = s
        .mailbox_cache
        .values()
        .find(|mb| mb.role.as_deref() == Some("drafts"))
        .ok_or_else(|| Error::Internal("No drafts mailbox found".into()))?
        .id
        .clone();

    let sent_id = s
        .mailbox_cache
        .values()
        .find(|mb| mb.role.as_deref() == Some("sent"))
        .ok_or_else(|| Error::Internal("No sent mailbox found".into()))?
        .id
        .clone();

    // Any calendar_ics (iTIP REQUEST invite or REPLY RSVP) can't be
    // expressed as Email/set bodyStructure — no part property carries the
    // MIME `method` parameter (kata vt0m for REPLY, kata 2xh9 for REQUEST:
    // bare `type` drops it, `type` with params / `header:Content-Type` /
    // blob Content-Type are all rejected or silently replaced) — so it goes
    // out as a client-built RFC822 message via Email/import. cc/bcc ride on
    // the envelope (and cc on a MIME header); attachments are downloaded and
    // inlined. `send_itip_via_import` rejects html_body/in_reply_to/references
    // (neither iTIP producer sets them).
    if sub.calendar_ics.is_some() {
        return send_itip_via_import(
            s,
            sub,
            from_addr,
            &account_id,
            &identity_id,
            &drafts_id,
            &sent_id,
        )
        .await;
    }

    let email_create = build_draft_email(sub, from_addr, &drafts_id);
    let rcpt_to = envelope_rcpt_to(sub);

    let resp = jmap_call(
        s,
        vec![
            serde_json::json!([
                "Email/set",
                {
                    "accountId": &account_id,
                    "create": {
                        "draft": email_create
                    }
                },
                "0"
            ]),
            {
                // Build the patch to move from Drafts → Sent and clear $draft keyword
                let mut patch = serde_json::Map::new();
                patch.insert(format!("mailboxIds/{drafts_id}"), serde_json::Value::Null);
                patch.insert(format!("mailboxIds/{sent_id}"), serde_json::json!(true));
                patch.insert("keywords/$draft".into(), serde_json::Value::Null);

                serde_json::json!([
                    "EmailSubmission/set",
                    {
                        "accountId": &account_id,
                        "create": {
                            "send": {
                                "emailId": "#draft",
                                "identityId": identity_id,
                                "envelope": {
                                    "mailFrom": { "email": from_addr },
                                    "rcptTo": rcpt_to
                                }
                            }
                        },
                        "onSuccessUpdateEmail": {
                            "#send": patch
                        }
                    },
                    "1"
                ])
            },
        ],
    )
    .await?;

    // Check for errors
    let email_created = &resp["methodResponses"][0][1]["created"]["draft"];
    if email_created.is_null() {
        return Err(Error::Internal(format!(
            "Email creation failed: {}",
            create_failure_detail(&resp, 0)
        )));
    }

    let submission = &resp["methodResponses"][1][1]["created"]["send"];
    if submission.is_null() {
        return Err(Error::Internal(format!(
            "Email submission failed: {}",
            create_failure_detail(&resp, 1)
        )));
    }

    // Return the email ID
    let email_id = submission["emailId"]
        .as_str()
        .or_else(|| email_created["id"].as_str())
        .map(String::from);

    Ok(email_id)
}

// =============================================================================
// Persistent drafts (kata wm57)
// =============================================================================
//
// Unlike the send flow's transient draft (created then immediately moved to
// Sent), these are drafts the user keeps: created in the Drafts mailbox with
// the `$draft` keyword and NO EmailSubmission, so nothing is dispatched. v1 is
// plain-text only — no html_body, no persisted attachments — so callers build
// the EmailSubmission with those fields empty and the body construction is
// reused from `build_draft_email` rather than duplicated.
//
// JMAP Email objects are immutable except for `mailboxIds`/`keywords`
// (RFC 8621): the body, subject and recipients of an existing draft cannot be
// patched in place. Editing a draft is therefore a destroy+recreate — one
// Email/set that creates the replacement and destroys the old id — which
// yields a NEW server id the caller must adopt (`update_draft` returns it).

/// Look up the Drafts mailbox id from the session cache, or error if the
/// account has no drafts mailbox. Mirrors the inline lookup in `send_email`.
fn drafts_mailbox_id(s: &JmapSession) -> Result<String, Error> {
    s.mailbox_cache
        .values()
        .find(|mb| mb.role.as_deref() == Some("drafts"))
        .map(|mb| mb.id.clone())
        .ok_or_else(|| Error::Internal("No drafts mailbox found".into()))
}

/// Email/set create for a persistent draft: `build_draft_email` plus the
/// `$draft` keyword, no submission. Pure so the request shape is testable.
fn draft_create_request(
    account_id: &str,
    sub: &EmailSubmission,
    from_addr: &str,
    drafts_mailbox_id: &str,
) -> Vec<serde_json::Value> {
    let mut email = build_draft_email(sub, from_addr, drafts_mailbox_id);
    email.insert("keywords".into(), serde_json::json!({ "$draft": true }));
    vec![serde_json::json!([
        "Email/set",
        {
            "accountId": account_id,
            "create": { "draft": email }
        },
        "0"
    ])]
}

/// Email/set destroy for a draft.
fn draft_destroy_request(account_id: &str, draft_id: &str) -> Vec<serde_json::Value> {
    vec![serde_json::json!([
        "Email/set",
        {
            "accountId": account_id,
            "destroy": [draft_id]
        },
        "0"
    ])]
}

/// Email/get fetching just enough to decide whether `draft_id` is safe to
/// destroy — the guard destroy_draft runs before issuing the raw (Trash-
/// bypassing) Email/set destroy above (roborev 302, fix 5).
fn draft_verify_request(account_id: &str, draft_id: &str) -> Vec<serde_json::Value> {
    vec![serde_json::json!([
        "Email/get",
        {
            "accountId": account_id,
            "ids": [draft_id],
            "properties": ["id", "mailboxIds", "keywords"]
        },
        "0"
    ])]
}

/// Decides, from a `draft_verify_request` response, whether `draft_id` is
/// safe to destroy: it must carry the `$draft` keyword or sit in the
/// Drafts mailbox. Email/set destroy has no Trash round-trip — it's
/// permanent — so a mismatch (an email that exists but is neither) is
/// refused with a `notFound`-style error rather than silently destroyed.
///
/// A target that no longer exists (absent from the response `list`) is
/// NOT a mismatch — that's left to the destroy call's own `notFound`
/// handling, which already treats "already gone" as idempotent success
/// (e.g. discarding a draft the send flow already deleted). Pure so the
/// decision logic is unit-testable without a network round-trip.
///
/// Fails closed on a malformed/error response (roborev 303, fix 3): a
/// method-level JMAP error (`["error", {...}, "0"]` — session invalidated,
/// rate limited, etc.) has no `list` at all, which used to fall through to
/// the "already gone" branch above and return `Ok(())` — an unverified
/// permanent destroy. Requiring the response to actually be an `Email/get`
/// result first means a lookup failure is treated as "can't confirm this is
/// a draft" rather than "must be already gone".
fn verify_is_draft_response(
    resp: &serde_json::Value,
    draft_id: &str,
    drafts_mailbox_id: &str,
) -> Result<(), Error> {
    if resp["methodResponses"][0][0].as_str() != Some("Email/get") {
        return Err(Error::Internal(format!(
            "draft verify lookup for {draft_id} failed: unexpected response {}",
            resp["methodResponses"][0]
        )));
    }
    let found = resp["methodResponses"][0][1]["list"]
        .as_array()
        .and_then(|list| list.iter().find(|e| e["id"].as_str() == Some(draft_id)));
    let Some(found) = found else {
        return Ok(());
    };
    let has_draft_keyword = found["keywords"]["$draft"].as_bool().unwrap_or(false);
    let in_drafts_mailbox = found["mailboxIds"]
        .as_object()
        .is_some_and(|m| m.contains_key(drafts_mailbox_id));
    if has_draft_keyword || in_drafts_mailbox {
        Ok(())
    } else {
        Err(Error::NotFound(format!(
            "{draft_id} is not a draft; refusing to destroy"
        )))
    }
}

/// Guard shared by every destroy_draft caller (the DELETE handler via
/// provider::destroy_draft, and update_draft's destroy-the-old-copy step —
/// both funnel through destroy_draft itself, so this one check covers
/// both) — see verify_is_draft_response for the rationale.
async fn verify_is_draft(s: &JmapSession, draft_id: &str) -> Result<(), Error> {
    let account_id = s.account_id.as_ref().ok_or(Error::NotConnected)?;
    let drafts_id = drafts_mailbox_id(s)?;
    let resp = jmap_call(s, draft_verify_request(account_id, draft_id)).await?;
    verify_is_draft_response(&resp, draft_id, &drafts_id)
}

/// Pull the created draft's id out of an Email/set response, or build an error
/// carrying the server's `notCreated` detail.
fn created_draft_id(resp: &serde_json::Value, action: &str) -> Result<String, Error> {
    let created = &resp["methodResponses"][0][1]["created"]["draft"];
    if let Some(id) = created["id"].as_str() {
        return Ok(id.to_string());
    }
    let not_created = &resp["methodResponses"][0][1]["notCreated"];
    let detail = if not_created.is_null() {
        "no detail".into()
    } else {
        not_created.to_string()
    };
    Err(Error::Internal(format!("{action} failed: {detail}")))
}

/// Create a persistent draft. Returns the new draft's server id.
pub async fn create_draft(
    s: &JmapSession,
    sub: &EmailSubmission,
    from_addr: &str,
) -> Result<String, Error> {
    let account_id = s.account_id.as_ref().ok_or(Error::NotConnected)?;
    let drafts_id = drafts_mailbox_id(s)?;
    let resp = jmap_call(
        s,
        draft_create_request(account_id, sub, from_addr, &drafts_id),
    )
    .await?;
    created_draft_id(&resp, "Draft creation")
}

/// Replace an existing draft (create-then-destroy). Returns the NEW server
/// id — callers must adopt it, as the old id is destroyed once the
/// replacement is confirmed.
///
/// Deliberately two sequential Email/set calls rather than one bundled
/// create+destroy: per RFC 8620, a failed create does not cancel a destroy
/// bundled into the same call, so a single combined request could destroy
/// the last good server copy on a failed update (roborev 294). The create is
/// issued and confirmed successful first; only then is the old id destroyed.
/// An unexpected destroy failure is logged, not propagated — the update
/// itself already succeeded and the caller has a new id to adopt.
pub async fn update_draft(
    s: &JmapSession,
    draft_id: &str,
    sub: &EmailSubmission,
    from_addr: &str,
) -> Result<String, Error> {
    let account_id = s.account_id.as_ref().ok_or(Error::NotConnected)?;
    let drafts_id = drafts_mailbox_id(s)?;

    let create_resp = jmap_call(
        s,
        draft_create_request(account_id, sub, from_addr, &drafts_id),
    )
    .await?;
    let new_id = created_draft_id(&create_resp, "Draft update")?;

    match destroy_draft(s, draft_id).await {
        Ok(true) => {}
        Ok(false) => {
            tracing::warn!(
                "Draft update: old draft {draft_id} was not destroyed after creating replacement {new_id} (unexpected notDestroyed)"
            );
        }
        Err(err) => {
            tracing::warn!(
                "Draft update: failed to destroy old draft {draft_id} after creating replacement {new_id}: {err}"
            );
        }
    }

    Ok(new_id)
}

/// Destroy a draft. Idempotent: a `notFound` (already gone) counts as success
/// so the fire-and-forget delete on send/discard never surfaces a spurious
/// error for a draft the send flow already removed.
///
/// Guarded by verify_is_draft (roborev 302, fix 5): Email/set destroy is
/// permanent, with no Trash round-trip, so before issuing it we confirm the
/// target actually carries the `$draft` keyword or sits in the Drafts
/// mailbox. Both destroy_draft call sites (the DELETE handler and
/// update_draft's destroy-the-old-copy step) go through this one function,
/// so the guard covers both.
pub async fn destroy_draft(s: &JmapSession, draft_id: &str) -> Result<bool, Error> {
    verify_is_draft(s, draft_id).await?;
    let account_id = s.account_id.as_ref().ok_or(Error::NotConnected)?;
    let resp = jmap_call(s, draft_destroy_request(account_id, draft_id)).await?;
    let destroyed = resp["methodResponses"][0][1]["destroyed"]
        .as_array()
        .is_some_and(|arr| arr.iter().any(|v| v.as_str() == Some(draft_id)));
    let already_gone = resp["methodResponses"][0][1]["notDestroyed"][draft_id]["type"].as_str()
        == Some("notFound");
    Ok(destroyed || already_gone)
}

// =============================================================================
// Calendar
// =============================================================================

pub fn find_calendar_blob_id(body_structure: &BodyStructurePart) -> Option<String> {
    let mime_type = body_structure.mime_type.to_lowercase();
    let filename = body_structure
        .name
        .as_deref()
        .unwrap_or_default()
        .to_lowercase();

    if mime_type == "text/calendar" || filename.ends_with(".ics") {
        return body_structure.blob_id.clone();
    }

    // Recurse into sub-parts
    for part in &body_structure.sub_parts {
        if let Some(blob_id) = find_calendar_blob_id(part) {
            return Some(blob_id);
        }
    }

    None
}

pub async fn get_calendar_data(s: &JmapSession, email_id: &str) -> Result<Option<String>, Error> {
    debug_assert!(!email_id.is_empty(), "email_id must not be empty");

    let account_id = s.account_id.as_ref().ok_or(Error::NotConnected)?;

    // Fetch body structure with blob IDs in a single call
    let resp = jmap_call(
        s,
        vec![serde_json::json!([
            "Email/get",
            {
                "accountId": account_id,
                "ids": [email_id],
                "properties": ["bodyStructure"],
                "bodyProperties": ["partId", "blobId", "type", "name", "subParts"]
            },
            "0"
        ])],
    )
    .await?;

    let list = resp["methodResponses"][0][1]["list"]
        .as_array()
        .ok_or_else(|| Error::Internal("Invalid JMAP Email/get response".into()))?;
    if list.is_empty() {
        return Err(Error::NotFound("Email not found".into()));
    }

    let body_structure: BodyStructurePart = match list[0].get("bodyStructure") {
        Some(v) if !v.is_null() => serde_json::from_value(v.clone())
            .map_err(|e| Error::Internal(format!("Failed to parse bodyStructure: {e}")))?,
        _ => return Ok(None),
    };
    let blob_id = match find_calendar_blob_id(&body_structure) {
        Some(id) => id,
        None => return Ok(None),
    };

    // Download the blob
    let download_url = s.download_url.as_ref().ok_or(Error::NotConnected)?;
    let url = download_url
        .replace("{accountId}", account_id)
        .replace("{blobId}", &blob_id)
        .replace("{name}", "invite.ics")
        .replace("{type}", "text/calendar");

    let resp = s
        .limiter
        .execute("blob.download", || async {
            s.client
                .get(&url)
                .header("Authorization", &s.auth_header)
                .send()
                .await
        })
        .await?;

    if !resp.status().is_success() {
        return Ok(None);
    }

    let ics_data = resp.text().await?;
    Ok(Some(ics_data))
}

/// `Err(CalendarAuthUnconfigured)` if no CalDAV app password is configured.
/// Called at the top of every CalDAV function so a missing credential is
/// surfaced as a named, actionable error *before* any HTTP request — not
/// swallowed as a 401 + `Ok(false)` + `warn!` (the bug that rotted silently for
/// months). Returns the `caldav_auth_header` (`Basic <base64(user:pass)>`) for
/// the call to send.
fn require_caldav_auth(s: &JmapSession) -> Result<&str, Error> {
    if s.caldav_auth_header.is_empty() {
        Err(Error::CalendarAuthUnconfigured)
    } else {
        Ok(&s.caldav_auth_header)
    }
}

/// Turn a non-2xx CalDAV response into an honest `Err` (not `Ok(false)`).
/// 401/403 → `Error::Auth`: the app password is present but wrong/revoked, an
/// actionable auth failure the UI surfaces as 401. Anything else →
/// `Error::Internal` with the status. The response body is logged at WARN for
/// operator debugging — it never reaches the client, since both `Error::Auth`
/// and `Error::Internal` redact their detail in `IntoResponse`.
fn caldav_failure(method: &str, url: &str, status: reqwest::StatusCode, body: &str) -> Error {
    tracing::warn!("CalDAV {method} {url} failed: {status} — {body}");
    if status.as_u16() == 401 || status.as_u16() == 403 {
        Error::Auth(format!("CalDAV {method} rejected ({status})"))
    } else {
        Error::Internal(format!("CalDAV {method} {url} failed: {status}"))
    }
}

// =============================================================================
// CalDAV calendar-home discovery (kata wybm)
// -----------------------------------------------------------------------------
// Replaces the hardcoded `/Default/` collection name that 301→404'd on real
// Fastmail accounts (every CalDAV write silently missed the calendar). The
// four CalDAV functions below now resolve the user's default *writable*
// calendar collection once via PROPFIND, cache it on the session, and address
// that cached URL — never a string-concatenated `/Default/`.
//
// THE CHAIN WAS VERIFIED EMPIRICALLY AGAINST THE LIVE Fastmail/CyrUS SERVER,
// not reasoned from the RFC — the vt0m lesson. The RFC 4791 §6.2.1 chain
// (PROPFIND principal → `calendar-home-set`) is DEAD on Fastmail: that
// property 404's on the principal, on `/dav/calendars`, and on the home
// itself (confirmed by `PROPFIND ...allprop` — Cyrus exposes only
// `displayname` + `principal` resourcetype there). What Fastmail/Cyrus DOES
// expose is `current-user-principal`, and — critically — its href encodes the
// Cyrus *underscore-munged* user segment (`matt_coburn@…`, NOT the dotted
// session username `matt.coburn@…`). The calendar home is the principal path
// with `principals`→`calendars` substituted. Cyrus tags the `calendar`
// resourcetype under RFC 4791's `urn:ietf:params:xml:ns:caldav` namespace (the
// standard — verified against the RFC text, §12.1; there is no other CalDAV
// namespace); the parser matches `calendar` by *local* element name so it's
// robust to the server's prefix choice (C:, X27CD:, …) and to a default-
// namespace serialization (`<calendar xmlns="urn:ietf:params:xml:ns:caldav"/>`).
// End-to-end verified: PUT/GET/DELETE through the resolved URL succeed
// (201/200/204/404).
// =============================================================================

/// PROPFIND body for step 1: ask the calendars root for
/// `current-user-principal` (the one property Fastmail/Cyrus exposes here).
const PROPFIND_PRINCIPAL_BODY: &str = r#"<?xml version="1.0" encoding="utf-8"?><D:propfind xmlns:D="DAV:"><D:prop><D:current-user-principal/></D:prop></D:propfind>"#;

/// PROPFIND body for step 3: ask the calendar home (Depth:1) for the
/// properties the parser needs to enumerate and pick a default writable
/// calendar collection — `resourcetype`, `displayname`, and
/// `current-user-privilege-set`.
const PROPFIND_HOME_BODY: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<D:propfind xmlns:D="DAV:"><D:prop><D:resourcetype/><D:displayname/><D:current-user-privilege-set/></D:prop></D:propfind>"#;

// --- XML parsing helpers ---
//
// Cyrus emits well-structured but namespace-prefix-variable XML (D:, C:,
// X27CD:, CY:, CS:, …) with CDATA-wrapped displaynames. These helpers match
// on *local* element names (the part after `:`, or the whole tag when
// un-prefixed) so the parser is immune to the server's prefix choices, and
// handle `<![CDATA[…]]>` text. Regexes are `LazyLock`-static to match the
// codebase style (`calendar.rs`) and avoid recompiling per call; discovery
// runs once per session anyway (cached).

static CUP_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?s)<(?:[A-Za-z0-9_.-]+:)?current-user-principal\b[^>]*>(.*?)</(?:[A-Za-z0-9_.-]+:)?current-user-principal\s*>")
        .expect("static regex")
});
static HREF_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?s)<(?:[A-Za-z0-9_.-]+:)?href\b[^>]*>(.*?)</(?:[A-Za-z0-9_.-]+:)?href\s*>")
        .expect("static regex")
});
static RESPONSE_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?s)<(?:[A-Za-z0-9_.-]+:)?response\b[^>]*>(.*?)</(?:[A-Za-z0-9_.-]+:)?response\s*>",
    )
    .expect("static regex")
});
static RESOURCETYPE_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?s)<(?:[A-Za-z0-9_.-]+:)?resourcetype\b[^>]*>(.*?)</(?:[A-Za-z0-9_.-]+:)?resourcetype\s*>")
        .expect("static regex")
});
static CALENDAR_ELEM_RE: LazyLock<Regex> = LazyLock::new(|| {
    // `<…:calendar .../>` or `<…:calendar ...>` — the `(?:\s[^>]*)?` after
    // `calendar` allows attributes (notably the default-namespace form
    // `<calendar xmlns="urn:ietf:params:xml:ns:caldav"/>`), while still
    // excluding `calendar-home-set` / `calendar-color` /
    // `supported-calendar-component-set` (those have `-` after `calendar`, not
    // whitespace or `/>`/`>`).
    Regex::new(r"<(?:[A-Za-z0-9_.-]+:)?calendar(?:\s[^>]*)?/?>").expect("static regex")
});
static SCHEDULE_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"<(?:[A-Za-z0-9_.-]+:)?schedule-(?:inbox|outbox)\b").expect("static regex")
});
static DISPLAYNAME_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?s)<(?:[A-Za-z0-9_.-]+:)?displayname\b[^>]*>(.*?)</(?:[A-Za-z0-9_.-]+:)?displayname\s*>",
    )
    .expect("static regex")
});
static PRIVILEGE_SET_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?s)<(?:[A-Za-z0-9_.-]+:)?current-user-privilege-set\b[^>]*>(.*?)</(?:[A-Za-z0-9_.-]+:)?current-user-privilege-set\s*>")
        .expect("static regex")
});
static WRITE_PRIV_RE: LazyLock<Regex> = LazyLock::new(|| {
    // `<…:write .../>` / `<…:write ...>` — allows attributes; does NOT match
    // `write-properties` / `write-content` (those have `-` after `write`).
    Regex::new(r"<(?:[A-Za-z0-9_.-]+:)?write(?:\s[^>]*)?/?>").expect("static regex")
});
static ALL_PRIV_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"<(?:[A-Za-z0-9_.-]+:)?all(?:\s[^>]*)?/?>").expect("static regex")
});
static CDATA_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?s)<!\[CDATA\[(.*?)\]\]>").expect("static regex"));

/// Strip a leading `<![CDATA[…]]>` wrapper if present, else trim whitespace.
/// Cyrus wraps `displayname` in CDATA; `href`/principal text is plain.
fn element_text(inner: &str) -> String {
    if let Some(c) = CDATA_RE.captures(inner) {
        return c[1].to_string();
    }
    inner.trim().to_string()
}

/// Step 1 parse: extract the `current-user-principal` href from the calendars
/// root PROPFIND response. Returns the server-absolute path (e.g.
/// `/dav/principals/user/matt_coburn@aristoi.ai/`), or `None` if the server
/// exposed no principal (a discovery failure the caller surfaces).
fn parse_current_user_principal(xml: &str) -> Option<String> {
    let cup = CUP_RE.captures(xml)?;
    let href_inner = HREF_RE.captures(&cup[1])?;
    let text = element_text(&href_inner[1]);
    if text.is_empty() { None } else { Some(text) }
}

/// Step 2: derive the calendar home path from the principal href by
/// substituting the `principals` segment for `calendars` (the Fastmail/Cyrus
/// convention — both live under `/dav/<collection-class>/user/{USER}/` with
/// the same user segment). `None` if the principal href has no `/principals/`
/// segment (a shape we don't recognize — surface rather than guess).
fn derive_calendar_home(principal_href: &str) -> Option<String> {
    if principal_href.contains("/principals/") {
        // `replacen` (not `replace`) so only the collection-class segment is
        // substituted — a server-controlled user segment that happened to
        // contain `/principals/` wouldn't be corrupted.
        Some(principal_href.replacen("/principals/", "/calendars/", 1))
    } else {
        None
    }
}

/// One calendar-home child as parsed from the Depth:1 PROPFIND — the fields
/// `pick_default_calendar` decides on. `href` is server-absolute with a
/// trailing slash.
#[derive(Clone, Debug)]
struct CalCollectionInfo {
    href: String,
    displayname: Option<String>,
    is_calendar: bool,
    is_schedule: bool,
    writable: bool,
}

/// Step 3 parse: enumerate the `<D:response>` blocks in the home PROPFIND and
/// distill each to a `CalCollectionInfo`. Non-calendar children (the home
/// itself, `schedule-inbox`/`schedule-outbox`) are included with their flags
/// set so `pick_default_calendar` can exclude them — matching the real Cyrus
/// home listing, which interleaves the home, calendars, and the scheduling
/// collections.
fn parse_calendar_collections(xml: &str) -> Vec<CalCollectionInfo> {
    RESPONSE_RE
        .find_iter(xml)
        .filter_map(|m| {
            let block = m.as_str();
            let href = element_text(&HREF_RE.captures(block)?[1]);
            let rt = RESOURCETYPE_RE
                .captures(block)
                .map(|c| c[1].to_string())
                .unwrap_or_default();
            let is_calendar = CALENDAR_ELEM_RE.is_match(&rt);
            let is_schedule = SCHEDULE_RE.is_match(&rt);
            let displayname = DISPLAYNAME_RE
                .captures(block)
                .map(|c| element_text(&c[1]))
                .filter(|s| !s.is_empty());
            let privs = PRIVILEGE_SET_RE
                .captures(block)
                .map(|c| c[1].to_string())
                .unwrap_or_default();
            let writable = WRITE_PRIV_RE.is_match(&privs) || ALL_PRIV_RE.is_match(&privs);
            Some(CalCollectionInfo {
                href,
                displayname,
                is_calendar,
                is_schedule,
                writable,
            })
        })
        .collect()
}

/// The last non-empty path segment of a collection href, with any trailing
/// slash trimmed — e.g. `/dav/calendars/user/u/u.Default/` → `u.Default`.
/// Used to recognize the Fastmail `username.Default` convention.
fn href_last_segment(href: &str) -> &str {
    href.trim_end_matches('/').rsplit('/').next().unwrap_or("")
}

/// Step 4: pick the default writable calendar collection from the parsed home
/// listing. Priority (most-specific to least), returning the first match —
/// honest failure (`None`) when nothing matches so the caller surfaces
/// `CalendarDiscoveryFailed` instead of guessing among multiple:
///   a. `displayname == "Default"` — a legacy/default-named calendar.
///   b. href segment `Default` or ending `.Default` — the Fastmail
///      `username.Default` convention (the old `/Default/` 301 target shape).
///   c. `displayname` contains `(Fastmail)` case-insensitively (and not
///      `task`) — Fastmail's system-default events calendar marker
///      (empirically `General (FastMail)`; the tasks calendar is
///      `DEFAULT_TASK_CALENDAR_NAME`, no marker). Case-insensitive because the
///      live value is `(FastMail)` but the brand is `Fastmail` — don't 503 a
///      multi-calendar account on a casing drift.
///   d. exactly one writable calendar collection — single-calendar accounts.
/// Schedule-inbox/outbox and non-writable collections are excluded throughout.
fn pick_default_calendar(colls: &[CalCollectionInfo]) -> Option<&CalCollectionInfo> {
    let writable: Vec<&CalCollectionInfo> = colls
        .iter()
        .filter(|c| c.is_calendar && c.writable && !c.is_schedule)
        .collect();
    if let Some(c) = writable
        .iter()
        .find(|c| c.displayname.as_deref() == Some("Default"))
    {
        return Some(c);
    }
    if let Some(c) = writable.iter().find(|c| {
        let seg = href_last_segment(&c.href);
        seg == "Default" || seg.ends_with(".Default")
    }) {
        return Some(c);
    }
    if let Some(c) = writable.iter().find(|c| {
        c.displayname.as_deref().is_some_and(|d| {
            let lower = d.to_lowercase();
            lower.contains("(fastmail)") && !lower.contains("task")
        })
    }) {
        return Some(c);
    }
    if writable.len() == 1 {
        return Some(writable[0]);
    }
    None
}

/// Resolve a server-returned href (server-absolute path, or a full URL) to an
/// absolute URL against the CalDAV base. Cyrus returns paths; this keeps the
/// parser honest if a full URL ever appears.
fn absolute_url(base: &str, href: &str) -> String {
    if href.starts_with("http://") || href.starts_with("https://") {
        href.to_string()
    } else {
        format!("{base}{href}")
    }
}

/// Issue one CalDAV PROPFIND. Returns the response body on 207 Multi-Status;
/// any other status (301/404/5xx — or a silently-followed redirect that landed
/// on a non-207) is a discovery failure, surfaced as
/// `CalendarDiscoveryFailed` rather than followed or swallowed. The 207 check
/// is the gate: reqwest follows redirects by default, but a PROPFIND that
/// 301's to a GET would return 200 (not 207) and surface here.
async fn caldav_propfind(
    s: &JmapSession,
    auth: &str,
    url: &str,
    depth: &str,
    body: &str,
) -> Result<String, Error> {
    let resp = s
        .client
        .request(reqwest::Method::from_bytes(b"PROPFIND").unwrap(), url)
        .header("Authorization", auth)
        .header("Depth", depth)
        .header("Content-Type", "application/xml; charset=utf-8")
        .body(body.to_string())
        .send()
        .await?;
    let status = resp.status();
    if status.as_u16() != 207 {
        let text = resp.text().await.unwrap_or_default();
        // Trim the body — it can be an HTML error page (Cyrus's 404 is ~460B);
        // the first line is usually the useful part. This detail is logged at
        // WARN by `CalendarDiscoveryFailed`'s `IntoResponse`, never sent to
        // the HTTP client, so a verbose body here is fine for operators.
        let detail = text.lines().next().unwrap_or("").trim();
        return Err(Error::CalendarDiscoveryFailed(format!(
            "PROPFIND {url} returned {status} — {detail}"
        )));
    }
    Ok(resp.text().await?)
}

/// Run the three-step discovery chain (no caching — the caller
/// `resolve_calendar_collection` caches the result) and return the absolute
/// default calendar collection URL (no trailing slash — the four callers
/// append `/{uid}.ics`). Each step's failure surfaces a specific
/// `CalendarDiscoveryFailed` message so the operator log names the broken
/// step.
async fn discover_calendar_collection(s: &JmapSession, auth: &str) -> Result<String, Error> {
    // Step 1: PROPFIND the calendars root for current-user-principal. We hit
    // `{caldav_base}/dav/calendars` directly rather than RFC 6764's
    // `/.well-known/caldav` because the well-known 301-redirects to exactly
    // this path, and reqwest (like browsers) drops the PROPFIND method/body
    // across a 301 — so PROPFINDing the well-known would arrive as a GET.
    // `/dav/calendars` is the redirect target and exposes
    // `current-user-principal` directly (verified live).
    let root = format!("{}/dav/calendars", s.caldav_base);
    let principal_xml = caldav_propfind(s, auth, &root, "0", PROPFIND_PRINCIPAL_BODY).await?;
    let principal_href = parse_current_user_principal(&principal_xml).ok_or_else(|| {
        Error::CalendarDiscoveryFailed(
            "PROPFIND of calendars root returned no current-user-principal".into(),
        )
    })?;

    // Step 2: derive the calendar home (principals → calendars).
    let home_path = derive_calendar_home(&principal_href).ok_or_else(|| {
        Error::CalendarDiscoveryFailed(format!(
            "cannot derive calendar home from principal href {principal_href}"
        ))
    })?;
    let home_url = absolute_url(&s.caldav_base, &home_path);

    // Step 3: PROPFIND the home Depth:1 and enumerate calendar collections.
    let home_xml = caldav_propfind(s, auth, &home_url, "1", PROPFIND_HOME_BODY).await?;
    let collections = parse_calendar_collections(&home_xml);
    let default = pick_default_calendar(&collections).ok_or_else(|| {
        Error::CalendarDiscoveryFailed(format!(
            "no writable default calendar collection found in {home_path} \
             ({} calendar collection(s) listed)",
            collections.iter().filter(|c| c.is_calendar).count()
        ))
    })?;
    // Strip the trailing slash so the four CalDAV functions can append
    // `/{uid}.ics` with a single slash — Cyrus lists collection hrefs with a
    // trailing `/`; keeping it would yield `…/COLLID//uid.ics`.
    let href = default.href.trim_end_matches('/');
    Ok(absolute_url(&s.caldav_base, href))
}

/// Resolve the session's default calendar collection URL, discovering it once
/// via PROPFIND and caching the result in `caldav_collection_url` for the
/// session lifetime (one discovery per session, not per CalDAV call).
/// `OnceCell::get_or_try_init` coordinates concurrent first-callers on the
/// same session — only one runs the two-PROPFIND discovery while the rest
/// await its result — so the `get_email` flow's fire-and-forget writers don't
/// each re-discover. On a cache hit no HTTP is issued. Discovery failure is
/// NOT cached — `get_or_try_init` leaves the cell empty on Err (and releases
/// the init permit on cancellation, no poisoning) so a transient failure
/// retries on the next call while a permanent one (no writable calendar)
/// keeps surfacing honestly each time. `auth` is the `require_caldav_auth`
/// header; discovery is only reached when the app password is configured (the
/// m5yp guard runs first in each caller).
async fn resolve_calendar_collection(s: &JmapSession, auth: &str) -> Result<String, Error> {
    // `get_or_try_init` runs `discover_calendar_collection` only if the cell
    // is empty; concurrent callers await the one in-flight init. `.clone()`
    // releases the shared borrow before the caller builds its per-call URL.
    Ok(s.caldav_collection_url
        .get_or_try_init(|| discover_calendar_collection(s, auth))
        .await?
        .clone())
}

/// Fetch the current calendar event from CalDAV by UID.
/// Returns a parsed CalendarEvent, or None if the event doesn't exist.
pub async fn get_calendar_event(
    s: &JmapSession,
    uid: &str,
    primary_tz: chrono_tz::Tz,
) -> Result<Option<CalendarEvent>, Error> {
    let auth = require_caldav_auth(s)?;
    // Resolve the default writable calendar collection via PROPFIND (cached on
    // the session) — never the hardcoded /Default/ that 301→404'd (kata wybm).
    let collection = resolve_calendar_collection(s, auth).await?;
    let caldav_url = format!("{}/{}.ics", collection, percent_encode_path(uid));

    let resp = s
        .client
        .get(&caldav_url)
        .header("Authorization", auth)
        .send()
        .await?;

    let status = resp.status();
    // 404 = not stored yet is a legitimate "no event", not a failure — the
    // get_email caller degrades to the email ICS. Other non-2xx surface.
    if status.as_u16() == 404 {
        return Ok(None);
    }
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        return Err(caldav_failure("GET", &caldav_url, status, &body));
    }

    let ics_data = resp.text().await?;
    Ok(calendar::parse_ics(&ics_data, primary_tz))
}

pub async fn add_to_calendar(
    s: &JmapSession,
    ics_data: &str,
    uid: &str,
    only_if_new: bool,
) -> Result<bool, Error> {
    // Missing app password → named error, no HTTP (kata m5yp).
    let auth = require_caldav_auth(s)?;
    // Strip METHOD before storing — RFC 4791: stored calendar objects must not
    // contain METHOD (it's an iTIP transport property, not a storage property)
    let ics_data = calendar::strip_method(ics_data);

    // Resolve the default writable calendar collection via PROPFIND (cached
    // on the session) — never the hardcoded /Default/ that 301→404'd (kata wybm).
    let collection = resolve_calendar_collection(s, auth).await?;
    // CalDAV PUT to the resolved calendar, using event UID as filename for idempotency
    let caldav_url = format!("{}/{}.ics", collection, percent_encode_path(uid));

    let mut req = s
        .client
        .put(&caldav_url)
        .header("Authorization", auth)
        .header("Content-Type", "text/calendar; charset=utf-8");

    // If-None-Match: * means "only create, don't overwrite existing"
    if only_if_new {
        req = req.header("If-None-Match", "*");
    }

    let resp = req.body(ics_data).send().await?;

    let status = resp.status();
    // If-None-Match: * + 412 Precondition Failed means the event already
    // exists. For only_if_new (the only path that sends the header) that's the
    // desired idempotent outcome — the event is in the calendar — not a
    // failure. Returning Ok(true) keeps the auto-add caller from logging a
    // spurious error on the benign lost-race; the explicit-add caller
    // (only_if_new = false, no If-None-Match) never sees a 412.
    if only_if_new && status.as_u16() == 412 {
        return Ok(true);
    }
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        return Err(caldav_failure("PUT", &caldav_url, status, &body));
    }

    Ok(true)
}

pub async fn remove_from_calendar(s: &JmapSession, uid: &str) -> Result<bool, Error> {
    // Missing app password → named error, no HTTP (kata m5yp).
    let auth = require_caldav_auth(s)?;
    // Resolved collection URL (cached) — never /Default/ (kata wybm).
    let collection = resolve_calendar_collection(s, auth).await?;
    let caldav_url = format!("{}/{}.ics", collection, percent_encode_path(uid));

    let resp = s
        .client
        .delete(&caldav_url)
        .header("Authorization", auth)
        .send()
        .await?;

    let status = resp.status();
    // DELETE is idempotent: 404 (already gone) is the desired end state, not a
    // failure. Surfacing it as Err would make a CANCEL for an already-removed
    // event or a double-decline 500.
    if status.as_u16() == 404 {
        return Ok(true);
    }
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        return Err(caldav_failure("DELETE", &caldav_url, status, &body));
    }

    Ok(true)
}

/// Read a stored event's attendee PARTSTAT for `attendee_email` from CalDAV.
/// Returns `Ok(None)` when the event isn't stored (404) or has no matching
/// attendee; `Err(CalendarAuthUnconfigured)` when no app password is set; and
/// `Err` (auth/internal) on other non-2xx — surfacing the failure rather than
/// the old silent `None`.
pub async fn get_rsvp_status(
    s: &JmapSession,
    uid: &str,
    attendee_email: &str,
    primary_tz: chrono_tz::Tz,
) -> Result<Option<String>, Error> {
    // Missing app password → named error, no HTTP (kata m5yp).
    let auth = require_caldav_auth(s)?;
    // Resolved collection URL (cached) — never /Default/ (kata wybm).
    let collection = resolve_calendar_collection(s, auth).await?;
    let caldav_url = format!("{}/{}.ics", collection, percent_encode_path(uid));

    let resp = s
        .client
        .get(&caldav_url)
        .header("Authorization", auth)
        .send()
        .await?;

    let status = resp.status();
    if status.as_u16() == 404 {
        return Ok(None);
    }
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        return Err(caldav_failure("GET", &caldav_url, status, &body));
    }

    let ics_data = resp.text().await?;
    Ok(attendee_status_from_ics(
        &ics_data,
        attendee_email,
        primary_tz,
    ))
}

/// Parse ICS data and extract a specific attendee's PARTSTAT.
fn attendee_status_from_ics(
    ics_data: &str,
    attendee_email: &str,
    primary_tz: chrono_tz::Tz,
) -> Option<String> {
    let event = crate::calendar::parse_ics(ics_data, primary_tz)?;
    let email_lower = attendee_email.to_lowercase();
    event
        .attendees
        .iter()
        .find(|a| a.email.to_lowercase() == email_lower)
        .map(|a| a.status.clone())
}

/// UUID v4 generation using /dev/urandom for proper randomness.
#[cfg(test)]
fn uuid_v4() -> String {
    let mut buf = [0u8; 16];
    // Read exactly 16 bytes from /dev/urandom
    let ok = (|| -> Result<(), std::io::Error> {
        use std::io::Read;
        let mut f = std::fs::File::open("/dev/urandom")?;
        f.read_exact(&mut buf)?;
        Ok(())
    })();
    if ok.is_err() {
        // Fallback: combine time + stack address + counter for entropy
        use std::sync::atomic::{AtomicU64, Ordering};
        use std::time::{SystemTime, UNIX_EPOCH};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let t = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let stack_addr = &buf as *const _ as u64;
        let count = COUNTER.fetch_add(1, Ordering::Relaxed);
        let seed = t ^ (stack_addr as u128) ^ ((count as u128) << 64);
        buf[..8].copy_from_slice(&(seed as u64).to_le_bytes());
        buf[8..].copy_from_slice(&((seed >> 64) as u64).to_le_bytes());
    }
    // Set version (4) and variant (10xx) bits per RFC 4122
    buf[6] = (buf[6] & 0x0F) | 0x40;
    buf[8] = (buf[8] & 0x3F) | 0x80;
    format!(
        "{:08x}-{:04x}-{:04x}-{:04x}-{:012x}",
        u32::from_be_bytes([buf[0], buf[1], buf[2], buf[3]]),
        u16::from_be_bytes([buf[4], buf[5]]),
        u16::from_be_bytes([buf[6], buf[7]]),
        u16::from_be_bytes([buf[8], buf[9]]),
        u64::from_be_bytes([0, 0, buf[10], buf[11], buf[12], buf[13], buf[14], buf[15]]),
    )
}

// =============================================================================
// Tests
// =============================================================================

/// A real loopback HTTP recorder for CalDAV and JMAP behavioral tests.
///
/// No mocking framework: `tokio::net::TcpListener` + `axum::serve` (the same
/// real-HTTP pattern the codebase already uses in `outlook.rs` tests). Every
/// incoming request is appended to the shared `recorded` buffer with its
/// method, path, `Authorization` / `Content-Type` headers, and body; the
/// server responds to *every* method/path with the canned `status` + `body`
/// so a test can drive PUT / GET / DELETE against one endpoint and then
/// assert on the recorded headers. A test points
/// `JmapSession::caldav_base` (CalDAV) or `api_url`/`upload_url` (JMAP) at
/// the returned URL so the functions under test hit the recorder instead of
/// Fastmail.
#[cfg(test)]
pub(crate) mod caldav_recorder {
    use std::sync::{Arc, Mutex};

    /// One request as seen by the loopback server.
    #[derive(Clone, Debug)]
    pub struct RecordedRequest {
        pub method: String,
        pub path: String,
        pub authorization: Option<String>,
        pub content_type: Option<String>,
        pub body: Vec<u8>,
    }

    /// Spawn the recorder. Returns `(base_url, recorded_buffer)`.
    pub async fn spawn(
        status: axum::http::StatusCode,
        body: Vec<u8>,
    ) -> (String, Arc<Mutex<Vec<RecordedRequest>>>) {
        let recorded: Arc<Mutex<Vec<RecordedRequest>>> = Arc::new(Mutex::new(Vec::new()));
        let recorded_for_handler = recorded.clone();
        let body_for_handler = body.clone();
        let app = axum::Router::new().fallback(move |req: axum::extract::Request| {
            let recorded = recorded_for_handler.clone();
            let body = body_for_handler.clone();
            async move {
                let method = req.method().to_string();
                let path = req.uri().path().to_string();
                let authorization = req
                    .headers()
                    .get("authorization")
                    .and_then(|v| v.to_str().ok())
                    .map(|s| s.to_string());
                let content_type = req
                    .headers()
                    .get("content-type")
                    .and_then(|v| v.to_str().ok())
                    .map(|s| s.to_string());
                let req_body = axum::body::to_bytes(req.into_body(), usize::MAX)
                    .await
                    .unwrap_or_default()
                    .to_vec();
                recorded.lock().unwrap().push(RecordedRequest {
                    method,
                    path,
                    authorization,
                    content_type,
                    body: req_body,
                });
                (status, axum::body::Bytes::from(body))
            }
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        (format!("http://{addr}"), recorded)
    }

    /// Convenience: the `Basic <base64(username:app_password)>` header the
    /// CalDAV functions are expected to send, so a test doesn't re-derive
    /// the encoding (and can't drift from the production encoding).
    pub fn expected_basic_header(username: &str, app_password: &str) -> String {
        use base64::Engine;
        format!(
            "Basic {}",
            base64::engine::general_purpose::STANDARD.encode(format!("{username}:{app_password}"))
        )
    }

    /// Spawn a *scripted* loopback recorder (kata wybm): the caller supplies a
    /// closure mapping `(method, path)` → `(status, body)`, so one server can
    /// answer the multi-step CalDAV discovery chain differently — PROPFIND on
    /// the calendars root returns the `current-user-principal` multistatus,
    /// PROPFIND on the derived home returns the collection list, and
    /// PUT/GET/DELETE on a collection member return the write/read/delete
    /// status. Every request is recorded (method/path/auth/content-type/body)
    /// so a test can assert the client followed the chain and addressed the
    /// *resolved* URL, never `/Default/`. No mocking framework — just a
    /// loopback `TcpListener` + axum, matching `spawn` above.
    pub async fn spawn_scripted<F>(responder: F) -> (String, Arc<Mutex<Vec<RecordedRequest>>>)
    where
        F: Fn(&str, &str) -> (axum::http::StatusCode, Vec<u8>) + Send + Sync + Clone + 'static,
    {
        let recorded: Arc<Mutex<Vec<RecordedRequest>>> = Arc::new(Mutex::new(Vec::new()));
        let recorded_for_handler = recorded.clone();
        let app = axum::Router::new().fallback(move |req: axum::extract::Request| {
            let recorded = recorded_for_handler.clone();
            let responder = responder.clone();
            async move {
                let method = req.method().to_string();
                let path = req.uri().path().to_string();
                let authorization = req
                    .headers()
                    .get("authorization")
                    .and_then(|v| v.to_str().ok())
                    .map(|s| s.to_string());
                let content_type = req
                    .headers()
                    .get("content-type")
                    .and_then(|v| v.to_str().ok())
                    .map(|s| s.to_string());
                let req_body = axum::body::to_bytes(req.into_body(), usize::MAX)
                    .await
                    .unwrap_or_default()
                    .to_vec();
                recorded.lock().unwrap().push(RecordedRequest {
                    method: method.clone(),
                    path: path.clone(),
                    authorization,
                    content_type,
                    body: req_body,
                });
                let (status, body) = responder(&method, &path);
                (status, axum::body::Bytes::from(body))
            }
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        (format!("http://{addr}"), recorded)
    }

    /// What kind of DAV resource a `CalCollection` is — drives the
    /// `resourcetype` the builder emits, so a test can mix real calendars with
    /// the schedule-inbox/outbox and the non-calendar home that Cyrus lists.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub enum CalKind {
        Calendar,
        ScheduleInbox,
        ScheduleOutbox,
        /// The home itself, or any plain `collection` (no `calendar`).
        PlainCollection,
    }

    /// One `<D:response>` the `home_multistatus` builder will emit — the fields
    /// the discovery parser reads (href / resourcetype / displayname /
    /// write-privilege). `displayname` is wrapped in CDATA like Cyrus.
    #[derive(Clone, Debug)]
    pub struct CalCollection {
        pub href: String,
        pub displayname: String,
        pub kind: CalKind,
        pub writable: bool,
    }

    /// Build the PROPFIND Depth:0 response for the calendars root
    /// (`{caldav_base}/dav/calendars`), carrying `current-user-principal`.
    /// `principal_href` is the server-absolute path Cyrus returns — the
    /// underscore-munged user segment (e.g. `/dav/principals/user/u_name@…/`),
    /// NOT the session username. This is step 1 of the verified chain.
    pub fn principal_multistatus(principal_href: &str) -> String {
        format!(
            "<?xml version=\"1.0\" encoding=\"utf-8\"?>\n\
<D:multistatus xmlns:D=\"DAV:\">\n  <D:response>\n    <D:href>/dav/calendars</D:href>\n    <D:propstat>\n      <D:prop>\n        <D:current-user-principal>\n          <D:href>{principal_href}</D:href>\n        </D:current-user-principal>\n      </D:prop>\n      <D:status>HTTP/1.1 200 OK</D:status>\n    </D:propstat>\n  </D:response>\n</D:multistatus>\n"
        )
    }

    /// Build the PROPFIND Depth:1 response for the calendar home, listing the
    /// given collections. Mimics the *real* Cyrus shape captured live so the
    /// parser is pinned against what the server actually emits: the `calendar`
    /// resourcetype is tagged under RFC 4791's `urn:ietf:params:xml:ns:caldav`
    /// namespace (Cyrus binds it to prefix `C:`), displaynames are CDATA-
    /// wrapped, and `current-user-privilege-set` carries `<D:write/>`
    /// (alongside `<D:write-properties/>` etc., to exercise the
    /// write-vs-`write-properties` distinction the parser must get right) when
    /// `writable` is true.
    pub fn home_multistatus(collections: &[CalCollection]) -> String {
        let mut out = String::from(
            "<?xml version=\"1.0\" encoding=\"utf-8\"?>\n\
<D:multistatus xmlns:D=\"DAV:\" xmlns:C=\"urn:ietf:params:xml:ns:caldav\" xmlns:CY=\"http://cyrusimap.org/ns/\">\n",
        );
        for c in collections {
            out.push_str("  <D:response>\n");
            out.push_str(&format!("    <D:href>{}</D:href>\n", c.href));
            out.push_str("    <D:propstat>\n      <D:prop>\n");
            // resourcetype — Cyrus tags calendar/schedule-* under RFC 4791's
            // `urn:ietf:params:xml:ns:caldav` namespace (C:), collection under D:.
            out.push_str("        <D:resourcetype>\n          <D:collection/>\n");
            match c.kind {
                CalKind::Calendar => out.push_str("          <C:calendar/>\n"),
                CalKind::ScheduleInbox => out.push_str("          <C:schedule-inbox/>\n"),
                CalKind::ScheduleOutbox => out.push_str("          <C:schedule-outbox/>\n"),
                CalKind::PlainCollection => {}
            }
            out.push_str("        </D:resourcetype>\n");
            out.push_str(&format!(
                "        <D:displayname><![CDATA[{}]]></D:displayname>\n",
                c.displayname
            ));
            // privileges — include the near-collision elements
            // (`write-properties`, `write-content`, `read`, `all`) so the
            // parser's `write`-vs-`write-properties` matching is exercised.
            out.push_str("        <D:current-user-privilege-set>\n");
            out.push_str("          <D:privilege><D:read/></D:privilege>\n");
            if c.writable {
                out.push_str("          <D:privilege><D:all/></D:privilege>\n");
                out.push_str("          <D:privilege><D:write/></D:privilege>\n");
                out.push_str("          <D:privilege><D:write-properties/></D:privilege>\n");
                out.push_str("          <D:privilege><D:write-content/></D:privilege>\n");
                out.push_str("          <D:privilege><CY:make-collection/></D:privilege>\n");
            }
            out.push_str("        </D:current-user-privilege-set>\n");
            out.push_str("      </D:prop>\n      <D:status>HTTP/1.1 200 OK</D:status>\n");
            out.push_str("    </D:propstat>\n  </D:response>\n");
        }
        out.push_str("</D:multistatus>\n");
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn deser_bs(json: serde_json::Value) -> BodyStructurePart {
        serde_json::from_value(json).unwrap()
    }

    // --- find_calendar_blob_id tests ---

    #[test]
    fn detect_text_calendar_mime() {
        let body = deser_bs(serde_json::json!({
            "type": "text/calendar",
            "blobId": "blob-cal-1"
        }));
        assert_eq!(find_calendar_blob_id(&body), Some("blob-cal-1".into()));
    }

    #[test]
    fn detect_ics_filename() {
        let body = deser_bs(serde_json::json!({
            "type": "application/octet-stream",
            "name": "invite.ics",
            "blobId": "blob-cal-2"
        }));
        assert_eq!(find_calendar_blob_id(&body), Some("blob-cal-2".into()));
    }

    #[test]
    fn detect_nested_calendar() {
        let body = deser_bs(serde_json::json!({
            "type": "multipart/alternative",
            "subParts": [
                { "type": "text/plain", "blobId": "blob-text" },
                { "type": "text/calendar", "blobId": "blob-cal-3" }
            ]
        }));
        assert_eq!(find_calendar_blob_id(&body), Some("blob-cal-3".into()));
    }

    #[test]
    fn no_calendar_returns_none() {
        let body = deser_bs(serde_json::json!({
            "type": "multipart/mixed",
            "subParts": [
                { "type": "text/plain", "blobId": "blob-text" },
                { "type": "text/html", "blobId": "blob-html" }
            ]
        }));
        assert_eq!(find_calendar_blob_id(&body), None);
    }

    #[test]
    fn null_body_returns_none() {
        assert_eq!(find_calendar_blob_id(&BodyStructurePart::default()), None);
    }

    #[test]
    fn empty_object_returns_none() {
        let body = deser_bs(serde_json::json!({}));
        assert_eq!(find_calendar_blob_id(&body), None);
    }

    #[test]
    fn top_level_calendar() {
        let body = deser_bs(serde_json::json!({
            "type": "text/calendar",
            "blobId": "blob-top"
        }));
        assert_eq!(find_calendar_blob_id(&body), Some("blob-top".into()));
    }

    #[test]
    fn case_insensitive_mime() {
        let body = deser_bs(serde_json::json!({
            "type": "Text/Calendar",
            "blobId": "blob-case"
        }));
        assert_eq!(find_calendar_blob_id(&body), Some("blob-case".into()));
    }

    #[test]
    fn case_insensitive_filename() {
        let body = deser_bs(serde_json::json!({
            "type": "application/octet-stream",
            "name": "Meeting.ICS",
            "blobId": "blob-case-file"
        }));
        assert_eq!(find_calendar_blob_id(&body), Some("blob-case-file".into()));
    }

    // --- find_attachments tests ---

    #[test]
    fn find_attachments_null_returns_empty() {
        assert!(find_attachments(&BodyStructurePart::default()).is_empty());
    }

    #[test]
    fn find_attachments_text_plain_skipped() {
        let body = deser_bs(serde_json::json!({
            "type": "text/plain",
            "blobId": "blob-1",
            "name": "body.txt"
        }));
        assert!(find_attachments(&body).is_empty());
    }

    #[test]
    fn find_attachments_pdf_with_disposition() {
        let body = deser_bs(serde_json::json!({
            "type": "application/pdf",
            "blobId": "blob-pdf",
            "name": "report.pdf",
            "size": 12345,
            "disposition": "attachment"
        }));
        let atts = find_attachments(&body);
        assert_eq!(atts.len(), 1);
        assert_eq!(atts[0].blob_id, "blob-pdf");
        assert_eq!(atts[0].name, "report.pdf");
        assert_eq!(atts[0].mime_type, "application/pdf");
        assert_eq!(atts[0].size, 12345);
    }

    #[test]
    fn find_attachments_by_filename_without_disposition() {
        let body = deser_bs(serde_json::json!({
            "type": "application/octet-stream",
            "blobId": "blob-bin",
            "name": "data.bin",
            "size": 100
        }));
        let atts = find_attachments(&body);
        assert_eq!(atts.len(), 1);
        assert_eq!(atts[0].name, "data.bin");
    }

    #[test]
    fn find_attachments_nested_multipart() {
        let body = deser_bs(serde_json::json!({
            "type": "multipart/mixed",
            "subParts": [
                { "type": "text/plain", "blobId": "blob-text" },
                { "type": "text/html", "blobId": "blob-html" },
                {
                    "type": "application/pdf",
                    "blobId": "blob-att",
                    "name": "invoice.pdf",
                    "size": 5000,
                    "disposition": "attachment"
                }
            ]
        }));
        let atts = find_attachments(&body);
        assert_eq!(atts.len(), 1);
        assert_eq!(atts[0].name, "invoice.pdf");
    }

    #[test]
    fn find_attachments_inline_skipped_in_related() {
        // Inline images inside multipart/related are HTML-embedded and should be skipped
        let body = deser_bs(serde_json::json!({
            "type": "multipart/related",
            "subParts": [
                {
                    "type": "text/html", "blobId": "b1", "partId": "1",
                    "subParts": []
                },
                {
                    "type": "image/png",
                    "blobId": "blob-img",
                    "name": "logo.png",
                    "size": 2000,
                    "disposition": "inline",
                    "subParts": []
                }
            ]
        }));
        assert!(find_attachments(&body).is_empty());
    }

    #[test]
    fn find_attachments_inline_in_mixed_included() {
        // Gmail marks user-attached photos as inline in multipart/mixed —
        // these should appear as downloadable attachments
        let body = deser_bs(serde_json::json!({
            "type": "multipart/mixed",
            "subParts": [
                {
                    "type": "image/jpeg",
                    "blobId": "blob-photo",
                    "name": "image0.jpeg",
                    "size": 148587,
                    "disposition": "inline",
                    "subParts": []
                },
                {
                    "type": "text/plain",
                    "blobId": "blob-text",
                    "partId": "2",
                    "size": 21,
                    "subParts": []
                }
            ]
        }));
        let atts = find_attachments(&body);
        assert_eq!(atts.len(), 1);
        assert_eq!(atts[0].name, "image0.jpeg");
    }

    #[test]
    fn find_attachments_mixed_inside_related_not_suppressed() {
        // A multipart/mixed nested inside multipart/related should NOT
        // suppress its inline attachments — in_related is scoped to
        // direct children only.
        let body = deser_bs(serde_json::json!({
            "type": "multipart/related",
            "subParts": [
                { "type": "text/html", "blobId": "b1", "partId": "1", "subParts": [] },
                {
                    "type": "multipart/mixed",
                    "subParts": [
                        {
                            "type": "image/png",
                            "blobId": "blob-photo",
                            "name": "photo.png",
                            "size": 5000,
                            "disposition": "inline",
                            "subParts": []
                        }
                    ]
                }
            ]
        }));
        let atts = find_attachments(&body);
        assert_eq!(atts.len(), 1);
        assert_eq!(atts[0].name, "photo.png");
    }

    #[test]
    fn find_attachments_no_blob_id_skipped() {
        let body = deser_bs(serde_json::json!({
            "type": "application/pdf",
            "name": "broken.pdf",
            "disposition": "attachment"
        }));
        assert!(find_attachments(&body).is_empty());
    }

    #[test]
    fn find_attachments_deeply_nested() {
        let body = deser_bs(serde_json::json!({
            "type": "multipart/mixed",
            "subParts": [
                {
                    "type": "multipart/alternative",
                    "subParts": [
                        { "type": "text/plain", "blobId": "b1" },
                        { "type": "text/html", "blobId": "b2" }
                    ]
                },
                {
                    "type": "multipart/mixed",
                    "subParts": [
                        {
                            "type": "image/jpeg",
                            "blobId": "blob-photo",
                            "name": "photo.jpg",
                            "size": 30000,
                            "disposition": "attachment"
                        },
                        {
                            "type": "application/zip",
                            "blobId": "blob-archive",
                            "name": "files.zip",
                            "size": 50000,
                            "disposition": "attachment"
                        }
                    ]
                }
            ]
        }));
        let atts = find_attachments(&body);
        assert_eq!(atts.len(), 2);
        assert_eq!(atts[0].name, "photo.jpg");
        assert_eq!(atts[1].name, "files.zip");
    }

    #[test]
    fn find_attachments_leaf_with_empty_subparts() {
        // JMAP returns "subParts": [] on leaf nodes, not absent.
        // This previously caused attachments to be missed because the code
        // treated any part with a subParts array as a multipart container.
        let body = deser_bs(serde_json::json!({
            "type": "multipart/mixed",
            "subParts": [
                {
                    "type": "multipart/related",
                    "subParts": [
                        {
                            "type": "multipart/alternative",
                            "subParts": [
                                { "type": "text/plain", "blobId": "b1", "partId": "1.1.1", "subParts": [] },
                                { "type": "text/html", "blobId": "b2", "partId": "1.1.2", "subParts": [] }
                            ]
                        },
                        {
                            "type": "image/jpeg", "blobId": "b3", "name": "inline.jpg",
                            "disposition": "inline", "size": 3560, "subParts": []
                        }
                    ]
                },
                {
                    "type": "application/pdf",
                    "blobId": "blob-pdf",
                    "name": "Benefits_Guide.pdf",
                    "disposition": "attachment",
                    "size": 739855,
                    "subParts": []
                }
            ]
        }));
        let atts = find_attachments(&body);
        assert_eq!(atts.len(), 1);
        assert_eq!(atts[0].name, "Benefits_Guide.pdf");
        assert_eq!(atts[0].size, 739855);
    }

    // --- percent_encode_path tests ---

    #[test]
    fn percent_encode_path_passes_safe_chars() {
        assert_eq!(percent_encode_path("logo.png"), "logo.png");
        assert_eq!(percent_encode_path("my-file_v2.jpg"), "my-file_v2.jpg");
    }

    #[test]
    fn percent_encode_path_encodes_spaces_and_special() {
        assert_eq!(percent_encode_path("my photo.png"), "my%20photo.png");
        assert_eq!(percent_encode_path("file#1.png"), "file%231.png");
        assert_eq!(percent_encode_path("a?b=c"), "a%3Fb%3Dc");
    }

    // --- attendee_status_from_ics tests ---

    const RSVP_TEST_ICS: &str = "\
BEGIN:VCALENDAR\r\n\
VERSION:2.0\r\n\
METHOD:REQUEST\r\n\
BEGIN:VEVENT\r\n\
UID:rsvp-test@example.com\r\n\
DTSTART:20260215T100000Z\r\n\
SUMMARY:Test\r\n\
ORGANIZER;CN=Alice:mailto:alice@example.com\r\n\
ATTENDEE;CN=Bob;PARTSTAT=ACCEPTED:mailto:bob@example.com\r\n\
ATTENDEE;CN=Carol;PARTSTAT=TENTATIVE:mailto:carol@example.com\r\n\
ATTENDEE;CN=Dave;PARTSTAT=NEEDS-ACTION:mailto:dave@example.com\r\n\
SEQUENCE:0\r\n\
END:VEVENT\r\n\
END:VCALENDAR";

    #[test]
    fn attendee_status_finds_accepted() {
        assert_eq!(
            attendee_status_from_ics(RSVP_TEST_ICS, "bob@example.com", chrono_tz::Tz::UTC),
            Some("ACCEPTED".into())
        );
    }

    #[test]
    fn attendee_status_finds_tentative() {
        assert_eq!(
            attendee_status_from_ics(RSVP_TEST_ICS, "carol@example.com", chrono_tz::Tz::UTC),
            Some("TENTATIVE".into())
        );
    }

    #[test]
    fn attendee_status_finds_needs_action() {
        assert_eq!(
            attendee_status_from_ics(RSVP_TEST_ICS, "dave@example.com", chrono_tz::Tz::UTC),
            Some("NEEDS-ACTION".into())
        );
    }

    #[test]
    fn attendee_status_case_insensitive_email() {
        assert_eq!(
            attendee_status_from_ics(RSVP_TEST_ICS, "Bob@Example.COM", chrono_tz::Tz::UTC),
            Some("ACCEPTED".into())
        );
    }

    #[test]
    fn attendee_status_unknown_email_returns_none() {
        assert_eq!(
            attendee_status_from_ics(RSVP_TEST_ICS, "nobody@example.com", chrono_tz::Tz::UTC,),
            None
        );
    }

    #[test]
    fn attendee_status_invalid_ics_returns_none() {
        assert_eq!(
            attendee_status_from_ics("not valid ics", "bob@example.com", chrono_tz::Tz::UTC),
            None
        );
    }

    // --- collect_inline_cids tests ---

    #[test]
    fn collect_inline_cids_finds_cid_parts() {
        let body = deser_bs(serde_json::json!({
            "type": "multipart/related",
            "subParts": [
                { "type": "text/html", "partId": "1", "blobId": "b1", "subParts": [] },
                {
                    "type": "image/png", "blobId": "blob-img1", "name": "logo.png",
                    "disposition": "inline", "cid": "logo123@example.com", "subParts": []
                },
                {
                    "type": "image/jpeg", "blobId": "blob-img2", "name": "photo.jpg",
                    "disposition": "inline", "cid": "photo456@example.com", "subParts": []
                }
            ]
        }));
        let mut cids = Vec::new();
        collect_inline_cids(&body, &mut cids);
        assert_eq!(cids.len(), 2);
        assert_eq!(cids[0].0, "logo123@example.com");
        assert_eq!(cids[0].1, "blob-img1");
        assert_eq!(cids[0].2, "logo.png");
        assert_eq!(cids[1].0, "photo456@example.com");
        assert_eq!(cids[1].1, "blob-img2");
    }

    #[test]
    fn collect_inline_cids_skips_parts_without_cid() {
        let body = deser_bs(serde_json::json!({
            "type": "image/png", "blobId": "b1", "name": "att.png",
            "disposition": "attachment", "subParts": []
        }));
        let mut cids = Vec::new();
        collect_inline_cids(&body, &mut cids);
        assert!(cids.is_empty());
    }

    #[test]
    fn collect_inline_cids_null_returns_empty() {
        let mut cids = Vec::new();
        collect_inline_cids(&BodyStructurePart::default(), &mut cids);
        assert!(cids.is_empty());
    }

    #[test]
    fn collect_inline_cids_defaults_name_to_inline() {
        let body = deser_bs(serde_json::json!({
            "type": "image/png", "blobId": "b1",
            "disposition": "inline", "cid": "abc@example.com", "subParts": []
        }));
        let mut cids = Vec::new();
        collect_inline_cids(&body, &mut cids);
        assert_eq!(cids.len(), 1);
        assert_eq!(cids[0].2, "inline");
    }

    // --- resolve_inline_cids tests ---

    #[test]
    fn resolve_inline_cids_prefix_cid_does_not_corrupt_longer_cid() {
        // Reproduces the bug: naive substring replacement of "cid:1" would
        // match inside "cid:12", turning it into "<url-for-1>2" instead of
        // leaving the "cid:12" reference to be replaced by its own URL.
        let html = r#"<img src="cid:12"><img src="cid:1">"#;
        let cids = vec![
            ("1".to_string(), "blob-1".to_string(), "one.png".to_string()),
            (
                "12".to_string(),
                "blob-12".to_string(),
                "twelve.png".to_string(),
            ),
        ];
        let result = resolve_inline_cids(html, "email-1", &cids);
        assert_eq!(
            result,
            r#"<img src="/api/emails/email-1/attachments/blob-12/twelve.png"><img src="/api/emails/email-1/attachments/blob-1/one.png">"#
        );
    }

    #[test]
    fn resolve_inline_cids_prefix_cid_regardless_of_input_order() {
        // Same as above but with cids supplied in the opposite order, to
        // confirm the fix sorts by length rather than relying on the
        // caller's (document) order.
        let html = r#"<img src="cid:1"><img src="cid:12">"#;
        let cids = vec![
            (
                "12".to_string(),
                "blob-12".to_string(),
                "twelve.png".to_string(),
            ),
            ("1".to_string(), "blob-1".to_string(), "one.png".to_string()),
        ];
        let result = resolve_inline_cids(html, "email-1", &cids);
        assert_eq!(
            result,
            r#"<img src="/api/emails/email-1/attachments/blob-1/one.png"><img src="/api/emails/email-1/attachments/blob-12/twelve.png">"#
        );
    }

    #[test]
    fn resolve_inline_cids_single_cid() {
        let html = r#"<img src="cid:abc">"#;
        let cids = vec![(
            "abc".to_string(),
            "blob-abc".to_string(),
            "a.png".to_string(),
        )];
        let result = resolve_inline_cids(html, "email-2", &cids);
        assert_eq!(
            result,
            r#"<img src="/api/emails/email-2/attachments/blob-abc/a.png">"#
        );
    }

    #[test]
    fn resolve_inline_cids_no_cids_is_noop() {
        let html = "<p>no images here</p>";
        let result = resolve_inline_cids(html, "email-3", &[]);
        assert_eq!(result, html);
    }

    // --- build_mailbox_move_patch tests (ticket ec9f) ---

    #[test]
    fn move_patch_adds_target_only_when_no_source() {
        let patch = build_mailbox_move_patch("mb-archive", None);
        assert_eq!(patch.len(), 1);
        assert_eq!(
            patch.get("mailboxIds/mb-archive"),
            Some(&serde_json::json!(true))
        );
    }

    #[test]
    fn move_patch_adds_target_and_removes_source() {
        let patch = build_mailbox_move_patch("mb-archive", Some("mb-inbox"));
        assert_eq!(patch.len(), 2);
        assert_eq!(
            patch.get("mailboxIds/mb-archive"),
            Some(&serde_json::json!(true))
        );
        assert_eq!(
            patch.get("mailboxIds/mb-inbox"),
            Some(&serde_json::Value::Null)
        );
    }

    #[test]
    fn move_patch_does_not_remove_source_equal_to_target() {
        // Moving "to" the mailbox it's already the source of (e.g. restoring
        // to Inbox when Inbox is both target and would-be source) must not
        // add-then-immediately-null the same path.
        let patch = build_mailbox_move_patch("mb-inbox", Some("mb-inbox"));
        assert_eq!(patch.len(), 1);
        assert_eq!(
            patch.get("mailboxIds/mb-inbox"),
            Some(&serde_json::json!(true))
        );
    }

    #[test]
    fn move_patch_is_a_patch_not_a_replace() {
        // The core regression this ticket fixes: the patch must never
        // contain a bare "mailboxIds" key (which JMAP treats as a full
        // property replacement, wiping unrelated mailbox memberships).
        let patch = build_mailbox_move_patch("mb-archive", Some("mb-inbox"));
        assert!(patch.get("mailboxIds").is_none());
        assert!(patch.keys().all(|k| k.starts_with("mailboxIds/")));
    }

    // --- build_draft_email tests ---

    fn simple_submission() -> EmailSubmission {
        EmailSubmission {
            to: vec!["bob@example.com".into()],
            cc: vec![],
            subject: "Test".into(),
            text_body: "Hello".into(),
            bcc: None,
            html_body: None,
            in_reply_to: None,
            references: None,
            attachments: vec![],
            calendar_ics: None,
        }
    }

    #[test]
    fn draft_includes_mailbox_ids() {
        let sub = simple_submission();
        let draft = build_draft_email(&sub, "alice@example.com", "mb-drafts-123");
        let ids = draft.get("mailboxIds").expect("mailboxIds must be present");
        assert_eq!(ids, &serde_json::json!({"mb-drafts-123": true}));
    }

    #[test]
    fn draft_forward_includes_mailbox_ids() {
        // Forward: no in_reply_to, subject starts with Fwd:
        let sub = EmailSubmission {
            to: vec!["charlie@example.com".into()],
            cc: vec![],
            subject: "Fwd: Important".into(),
            text_body: "---------- Forwarded message ---------\n...".into(),
            bcc: None,
            html_body: None,
            in_reply_to: None,
            references: None,
            attachments: vec![],
            calendar_ics: None,
        };
        let draft = build_draft_email(&sub, "alice@example.com", "mb-drafts-456");
        let ids = draft.get("mailboxIds").expect("mailboxIds must be present");
        assert_eq!(ids, &serde_json::json!({"mb-drafts-456": true}));
    }

    #[test]
    fn draft_reply_includes_mailbox_ids() {
        let sub = EmailSubmission {
            to: vec!["bob@example.com".into()],
            cc: vec![],
            subject: "Re: Hello".into(),
            text_body: "Reply body".into(),
            bcc: None,
            html_body: None,
            in_reply_to: Some("<msg-123@example.com>".into()),
            references: Some(vec!["<msg-123@example.com>".into()]),
            attachments: vec![],
            calendar_ics: None,
        };
        let draft = build_draft_email(&sub, "alice@example.com", "mb-drafts-789");
        assert!(draft.contains_key("mailboxIds"));
        assert!(draft.contains_key("inReplyTo"));
        assert!(draft.contains_key("references"));
    }

    #[test]
    fn draft_sets_from_to_subject_body() {
        let sub = simple_submission();
        let draft = build_draft_email(&sub, "alice@example.com", "mb-drafts");
        assert_eq!(
            draft["from"],
            serde_json::json!([{"email": "alice@example.com"}])
        );
        assert_eq!(
            draft["to"],
            serde_json::json!([{"email": "bob@example.com"}])
        );
        assert_eq!(draft["subject"], serde_json::json!("Test"));
    }

    #[test]
    fn draft_omits_empty_cc_and_bcc() {
        let sub = simple_submission();
        let draft = build_draft_email(&sub, "a@b.com", "mb");
        assert!(!draft.contains_key("cc"));
        assert!(!draft.contains_key("bcc"));
    }

    #[test]
    fn draft_includes_cc_and_bcc_when_present() {
        let sub = EmailSubmission {
            to: vec!["bob@example.com".into()],
            cc: vec!["cc@example.com".into()],
            subject: "Test".into(),
            text_body: "Hello".into(),
            bcc: Some(vec!["bcc@example.com".into()]),
            html_body: None,
            in_reply_to: None,
            references: None,
            attachments: vec![],
            calendar_ics: None,
        };
        let draft = build_draft_email(&sub, "a@b.com", "mb");
        assert_eq!(
            draft["cc"],
            serde_json::json!([{"email": "cc@example.com"}])
        );
        assert_eq!(
            draft["bcc"],
            serde_json::json!([{"email": "bcc@example.com"}])
        );
    }

    #[test]
    fn drafts_mailbox_lookup_fails_when_missing() {
        let cache: HashMap<String, Mailbox> = HashMap::from([(
            "inbox-id".into(),
            Mailbox {
                id: "inbox-id".into(),
                name: "Inbox".into(),
                role: Some("inbox".into()),
                total_emails: 0,
                unread_emails: 0,
                parent_id: None,
            },
        )]);
        let result = cache
            .values()
            .find(|mb| mb.role.as_deref() == Some("drafts"));
        assert!(
            result.is_none(),
            "should not find drafts in cache without one"
        );
    }

    #[test]
    fn drafts_mailbox_lookup_succeeds() {
        let cache: HashMap<String, Mailbox> = HashMap::from([
            (
                "inbox-id".into(),
                Mailbox {
                    id: "inbox-id".into(),
                    name: "Inbox".into(),
                    role: Some("inbox".into()),
                    total_emails: 0,
                    unread_emails: 0,
                    parent_id: None,
                },
            ),
            (
                "drafts-id".into(),
                Mailbox {
                    id: "drafts-id".into(),
                    name: "Drafts".into(),
                    role: Some("drafts".into()),
                    total_emails: 0,
                    unread_emails: 0,
                    parent_id: None,
                },
            ),
        ]);
        let result = cache
            .values()
            .find(|mb| mb.role.as_deref() == Some("drafts"));
        assert_eq!(result.unwrap().id, "drafts-id");
    }

    // --- persistent draft request-shape tests (kata wm57) ---

    fn draft_set_args(calls: &[serde_json::Value]) -> &serde_json::Value {
        // Every draft request is a single Email/set method call.
        assert_eq!(calls.len(), 1, "draft requests are a single method call");
        assert_eq!(calls[0][0], "Email/set", "draft persistence uses Email/set");
        &calls[0][1]
    }

    #[test]
    fn draft_create_request_has_draft_keyword_and_mailbox() {
        let sub = simple_submission();
        let calls = draft_create_request("acct-1", &sub, "alice@example.com", "mb-drafts");
        let args = draft_set_args(&calls);
        assert_eq!(args["accountId"], "acct-1");
        let create = &args["create"]["draft"];
        assert_eq!(
            create["keywords"],
            serde_json::json!({ "$draft": true }),
            "a persistent draft must carry the $draft keyword"
        );
        assert_eq!(
            create["mailboxIds"],
            serde_json::json!({ "mb-drafts": true }),
            "the draft must land in the Drafts mailbox"
        );
        assert_eq!(create["subject"], "Test");
        assert_eq!(
            create["to"],
            serde_json::json!([{"email": "bob@example.com"}])
        );
    }

    #[test]
    fn draft_create_request_has_no_submission() {
        // The distinguishing feature vs the send flow: no EmailSubmission,
        // so nothing is dispatched — the draft just sits in Drafts.
        let sub = simple_submission();
        let calls = draft_create_request("acct-1", &sub, "alice@example.com", "mb-drafts");
        let raw = serde_json::to_string(&calls).unwrap();
        assert!(
            !raw.contains("EmailSubmission"),
            "creating a draft must not issue an EmailSubmission"
        );
        let args = draft_set_args(&calls);
        assert!(
            args.get("destroy").is_none(),
            "a plain create must not destroy anything"
        );
    }

    // update_draft (roborev 294: safe update ordering) issues the create and
    // destroy as two independent Email/set calls rather than one bundled
    // request, so a failed create can't take the old draft down with it.
    // draft_create_request_has_no_submission already asserts a create never
    // carries a "destroy", and draft_destroy_request_targets_the_id asserts a
    // destroy never carries a "create" — together they lock in that the two
    // steps stay separate. This test pins the create-step shape specifically
    // for the update path: same $draft keyword as a plain create, still no
    // destroy bundled in, using the OLD draft id as context (it's the create
    // that must succeed before that id is ever touched).
    #[test]
    fn draft_update_create_step_has_no_bundled_destroy() {
        let sub = simple_submission();
        // update_draft's create step reuses draft_create_request verbatim —
        // the old draft id plays no part in building it.
        let calls = draft_create_request("acct-1", &sub, "alice@example.com", "mb-drafts");
        let args = draft_set_args(&calls);
        assert_eq!(
            args["create"]["draft"]["keywords"],
            serde_json::json!({ "$draft": true })
        );
        assert!(
            args.get("destroy").is_none(),
            "the create step of an update must not bundle a destroy of the old draft \
             — the old id is only destroyed after this create is confirmed"
        );
    }

    // update_draft's destroy step reuses draft_destroy_request verbatim — it
    // runs only after the create above is confirmed, and targets exactly the
    // old draft id, nothing else.
    #[test]
    fn draft_update_destroy_step_targets_old_id_only() {
        let calls = draft_destroy_request("acct-1", "old-draft-id");
        let args = draft_set_args(&calls);
        assert_eq!(args["destroy"], serde_json::json!(["old-draft-id"]));
        assert!(
            args.get("create").is_none(),
            "the destroy step of an update must not bundle a create — by the time it \
             runs, the replacement draft already exists from the prior call"
        );
    }

    #[test]
    fn draft_destroy_request_targets_the_id() {
        let calls = draft_destroy_request("acct-1", "draft-xyz");
        let args = draft_set_args(&calls);
        assert_eq!(args["accountId"], "acct-1");
        assert_eq!(args["destroy"], serde_json::json!(["draft-xyz"]));
        assert!(args.get("create").is_none());
    }

    // --- destroy_draft $draft guard (roborev 302, fix 5) ---

    #[test]
    fn draft_verify_request_fetches_id_and_guard_properties() {
        let calls = draft_verify_request("acct-1", "draft-xyz");
        assert_eq!(calls.len(), 1, "verify is a single method call");
        assert_eq!(calls[0][0], "Email/get");
        let args = &calls[0][1];
        assert_eq!(args["accountId"], "acct-1");
        assert_eq!(args["ids"], serde_json::json!(["draft-xyz"]));
        let props = args["properties"].as_array().expect("properties array");
        for want in ["id", "mailboxIds", "keywords"] {
            assert!(
                props.iter().any(|p| p == want),
                "properties must include {want}"
            );
        }
    }

    fn email_get_list_response(items: Vec<serde_json::Value>) -> serde_json::Value {
        serde_json::json!({
            "methodResponses": [["Email/get", { "list": items, "notFound": [] }, "0"]]
        })
    }

    #[test]
    fn verify_is_draft_response_allows_draft_keyword() {
        let resp = email_get_list_response(vec![serde_json::json!({
            "id": "draft-xyz",
            "mailboxIds": {"mb-inbox": true},
            "keywords": {"$draft": true}
        })]);
        assert!(verify_is_draft_response(&resp, "draft-xyz", "mb-drafts").is_ok());
    }

    #[test]
    fn verify_is_draft_response_allows_drafts_mailbox_without_keyword() {
        // Some servers may not round-trip the $draft keyword faithfully;
        // sitting in the Drafts mailbox is an equally valid signal.
        let resp = email_get_list_response(vec![serde_json::json!({
            "id": "draft-xyz",
            "mailboxIds": {"mb-drafts": true},
            "keywords": {}
        })]);
        assert!(verify_is_draft_response(&resp, "draft-xyz", "mb-drafts").is_ok());
    }

    #[test]
    fn verify_is_draft_response_rejects_non_draft_mismatch() {
        // Neither the keyword nor Drafts-mailbox membership — this is the
        // scenario the guard exists to catch: a raw destroy would otherwise
        // permanently delete an arbitrary, non-draft email.
        let resp = email_get_list_response(vec![serde_json::json!({
            "id": "inbox-email-1",
            "mailboxIds": {"mb-inbox": true},
            "keywords": {"$seen": true}
        })]);
        let err = verify_is_draft_response(&resp, "inbox-email-1", "mb-drafts").unwrap_err();
        assert!(matches!(err, Error::NotFound(ref m) if m.contains("inbox-email-1")));
    }

    #[test]
    fn verify_is_draft_response_allows_already_gone_target() {
        // Absent from `list` entirely — left to the destroy call's own
        // idempotent notFound handling, not a mismatch.
        let resp = email_get_list_response(vec![]);
        assert!(verify_is_draft_response(&resp, "already-gone", "mb-drafts").is_ok());
    }

    #[test]
    fn verify_is_draft_response_fails_closed_on_error_response() {
        // roborev 303, fix 3: a method-level JMAP error (session invalidated,
        // rate limited, etc.) leaves methodResponses[0] shaped as
        // ["error", {...}, "0"] rather than ["Email/get", {"list": [...]}, "0"].
        // Before the fix, `["list"]` on the error's argument object came back
        // null, `.as_array()` was None, `found` was None, and the function
        // returned Ok(()) via the "already gone" branch — an unverified
        // permanent destroy. It must now refuse instead of guessing.
        let resp = serde_json::json!({
            "methodResponses": [["error", {"type": "serverFail"}, "0"]]
        });
        let err = verify_is_draft_response(&resp, "draft-xyz", "mb-drafts").unwrap_err();
        assert!(
            matches!(err, Error::Internal(ref m) if m.contains("draft-xyz")),
            "a malformed/error lookup response must fail closed with an error, not Ok(())"
        );
    }

    #[test]
    fn created_draft_id_reads_new_id() {
        let resp = serde_json::json!({
            "methodResponses": [["Email/set", { "created": { "draft": { "id": "new-id-1" } } }, "0"]]
        });
        assert_eq!(
            created_draft_id(&resp, "Draft creation").unwrap(),
            "new-id-1"
        );
    }

    #[test]
    fn created_draft_id_surfaces_not_created() {
        let resp = serde_json::json!({
            "methodResponses": [["Email/set", {
                "created": {},
                "notCreated": { "draft": { "type": "invalidProperties" } }
            }, "0"]]
        });
        let err = created_draft_id(&resp, "Draft creation").unwrap_err();
        assert!(matches!(err, Error::Internal(ref m) if m.contains("Draft creation failed")));
    }

    #[test]
    fn parse_email_in_reply_to_takes_first_message_id() {
        // The restore path needs the draft's threading parent back out of the
        // fetch (kata wm57 review follow-up). JMAP inReplyTo is String[]|null.
        let item = serde_json::json!({
            "id": "e1",
            "inReplyTo": ["<parent@example.com>", "<older@example.com>"]
        });
        let email = parse_jmap_email(&item, false);
        assert_eq!(email.in_reply_to.as_deref(), Some("<parent@example.com>"));
    }

    #[test]
    fn parse_email_in_reply_to_absent_or_null_is_none() {
        let absent = parse_jmap_email(&serde_json::json!({ "id": "e1" }), false);
        assert_eq!(absent.in_reply_to, None);
        let null = parse_jmap_email(&serde_json::json!({ "id": "e1", "inReplyTo": null }), false);
        assert_eq!(null.in_reply_to, None);
    }

    // --- parse_jmap_email tests (THE-153) ---

    #[test]
    fn parse_single_text_body_part() {
        let item = serde_json::json!({
            "id": "email-1",
            "blobId": "blob-1",
            "threadId": "thread-1",
            "mailboxIds": {"inbox-id": true},
            "keywords": {"$seen": true},
            "receivedAt": "2024-01-15T10:30:00Z",
            "subject": "Hello",
            "from": [{"name": "Alice", "email": "alice@example.com"}],
            "to": [{"name": "Bob", "email": "bob@example.com"}],
            "cc": [],
            "preview": "Hello there",
            "hasAttachment": false,
            "size": 500,
            "textBody": [{"partId": "1", "type": "text/plain"}],
            "htmlBody": [],
            "bodyValues": {
                "1": {"value": "Hello there"}
            },
            "bodyStructure": {"type": "text/plain"}
        });
        let email = parse_jmap_email(&item, true);
        assert_eq!(email.text_body, Some("Hello there".into()));
        assert_eq!(email.html_body, None);
    }

    #[test]
    fn parse_single_html_body_part() {
        let item = serde_json::json!({
            "id": "email-2",
            "blobId": "blob-2",
            "threadId": "thread-2",
            "mailboxIds": {},
            "keywords": {},
            "receivedAt": "2024-01-15T10:30:00Z",
            "subject": "HTML Email",
            "from": [{"email": "alice@example.com"}],
            "to": [{"email": "bob@example.com"}],
            "cc": [],
            "preview": "Hello",
            "hasAttachment": false,
            "size": 800,
            "textBody": [],
            "htmlBody": [{"partId": "1", "type": "text/html"}],
            "bodyValues": {
                "1": {"value": "<p>Hello</p>"}
            },
            "bodyStructure": {"type": "text/html"}
        });
        let email = parse_jmap_email(&item, true);
        assert_eq!(email.text_body, None);
        assert_eq!(email.html_body, Some("<p>Hello</p>".into()));
    }

    #[test]
    fn parse_both_text_and_html_single_parts() {
        let item = serde_json::json!({
            "id": "email-3",
            "blobId": "blob-3",
            "threadId": "thread-3",
            "mailboxIds": {"inbox": true},
            "keywords": {},
            "receivedAt": "2024-01-15T10:30:00Z",
            "subject": "Both Bodies",
            "from": [{"email": "alice@example.com"}],
            "to": [{"email": "bob@example.com"}],
            "cc": [],
            "preview": "Preview",
            "hasAttachment": false,
            "size": 1000,
            "textBody": [{"partId": "t1", "type": "text/plain"}],
            "htmlBody": [{"partId": "h1", "type": "text/html"}],
            "bodyValues": {
                "t1": {"value": "Plain text version"},
                "h1": {"value": "<p>HTML version</p>"}
            },
            "bodyStructure": {"type": "multipart/alternative"}
        });
        let email = parse_jmap_email(&item, true);
        assert_eq!(email.text_body, Some("Plain text version".into()));
        assert_eq!(email.html_body, Some("<p>HTML version</p>".into()));
    }

    #[test]
    fn parse_no_body_when_fetch_body_false() {
        let item = serde_json::json!({
            "id": "email-4",
            "blobId": "blob-4",
            "threadId": "thread-4",
            "mailboxIds": {},
            "keywords": {},
            "receivedAt": "2024-01-15T10:30:00Z",
            "subject": "No Body",
            "from": [{"email": "alice@example.com"}],
            "to": [{"email": "bob@example.com"}],
            "cc": [],
            "preview": "Preview",
            "hasAttachment": false,
            "size": 200,
            "textBody": [{"partId": "1"}],
            "htmlBody": [{"partId": "2"}],
            "bodyValues": {
                "1": {"value": "Text"},
                "2": {"value": "<p>HTML</p>"}
            },
            "bodyStructure": {"type": "multipart/alternative"}
        });
        let email = parse_jmap_email(&item, false);
        assert_eq!(email.text_body, None);
        assert_eq!(email.html_body, None);
    }

    #[test]
    fn parse_email_resolves_cid_urls() {
        let item = serde_json::json!({
            "id": "email-cid",
            "blobId": "blob-cid",
            "threadId": "thread-cid",
            "mailboxIds": {"inbox": true},
            "keywords": {},
            "receivedAt": "2024-01-15T10:30:00Z",
            "subject": "Inline Images",
            "from": [{"email": "alice@example.com"}],
            "to": [{"email": "bob@example.com"}],
            "cc": [],
            "preview": "Preview",
            "hasAttachment": false,
            "size": 5000,
            "textBody": [],
            "htmlBody": [{"partId": "1", "type": "text/html"}],
            "bodyValues": {
                "1": {"value": "<p>Hello</p><img src=\"cid:logo123@example.com\">"}
            },
            "bodyStructure": {
                "type": "multipart/related",
                "subParts": [
                    { "type": "text/html", "partId": "1", "blobId": "b1", "subParts": [] },
                    {
                        "type": "image/png", "blobId": "blob-img1", "name": "logo.png",
                        "disposition": "inline", "cid": "logo123@example.com", "subParts": []
                    }
                ]
            }
        });
        let email = parse_jmap_email(&item, true);
        let html = email.html_body.unwrap();
        assert!(!html.contains("cid:"), "cid: references should be resolved");
        assert!(
            html.contains("/api/emails/email-cid/attachments/blob-img1/logo.png"),
            "should contain download URL, got: {html}"
        );
    }

    #[test]
    fn parse_email_no_cid_unchanged() {
        let item = serde_json::json!({
            "id": "email-nocid",
            "blobId": "blob-nocid",
            "threadId": "thread-nocid",
            "mailboxIds": {},
            "keywords": {},
            "receivedAt": "2024-01-15T10:30:00Z",
            "subject": "No CID",
            "from": [{"email": "alice@example.com"}],
            "to": [{"email": "bob@example.com"}],
            "cc": [],
            "preview": "Preview",
            "hasAttachment": false,
            "size": 500,
            "textBody": [],
            "htmlBody": [{"partId": "1", "type": "text/html"}],
            "bodyValues": {
                "1": {"value": "<p>No inline images</p>"}
            },
            "bodyStructure": {"type": "text/html"}
        });
        let email = parse_jmap_email(&item, true);
        assert_eq!(email.html_body, Some("<p>No inline images</p>".into()));
    }

    #[test]
    fn parse_email_cid_with_special_filename() {
        let item = serde_json::json!({
            "id": "email-sp",
            "blobId": "blob-sp",
            "threadId": "thread-sp",
            "mailboxIds": {},
            "keywords": {},
            "receivedAt": "2024-01-15T10:30:00Z",
            "subject": "Special Filename",
            "from": [{"email": "alice@example.com"}],
            "to": [{"email": "bob@example.com"}],
            "cc": [],
            "preview": "Preview",
            "hasAttachment": false,
            "size": 500,
            "textBody": [],
            "htmlBody": [{"partId": "1", "type": "text/html"}],
            "bodyValues": {
                "1": {"value": "<img src=\"cid:sp@example.com\">"}
            },
            "bodyStructure": {
                "type": "multipart/related",
                "subParts": [
                    { "type": "text/html", "partId": "1", "blobId": "b1", "subParts": [] },
                    {
                        "type": "image/png", "blobId": "blob-sp1", "name": "my photo.png",
                        "disposition": "inline", "cid": "sp@example.com", "subParts": []
                    }
                ]
            }
        });
        let email = parse_jmap_email(&item, true);
        let html = email.html_body.unwrap();
        assert!(
            html.contains("my%20photo.png"),
            "filename with spaces should be percent-encoded, got: {html}"
        );
    }

    #[test]
    fn parse_multiple_text_body_parts_concatenated() {
        // AC-1: Forwarded/reply emails often have multiple body parts.
        // All parts should be concatenated, not just the first.
        let item = serde_json::json!({
            "id": "email-5",
            "blobId": "blob-5",
            "threadId": "thread-5",
            "mailboxIds": {},
            "keywords": {},
            "receivedAt": "2024-01-15T10:30:00Z",
            "subject": "Fwd: Original",
            "from": [{"email": "alice@example.com"}],
            "to": [{"email": "bob@example.com"}],
            "cc": [],
            "preview": "See below",
            "hasAttachment": false,
            "size": 1200,
            "textBody": [
                {"partId": "1", "type": "text/plain"},
                {"partId": "2", "type": "text/plain"}
            ],
            "htmlBody": [],
            "bodyValues": {
                "1": {"value": "See below forwarded message."},
                "2": {"value": "This is the original message text."}
            },
            "bodyStructure": {"type": "multipart/mixed"}
        });
        let email = parse_jmap_email(&item, true);
        let text = email.text_body.expect("text_body should be Some");
        assert!(
            text.contains("See below forwarded message."),
            "Should contain first part: {text}"
        );
        assert!(
            text.contains("This is the original message text."),
            "Should contain second part: {text}"
        );
        // Parts should be separated by a newline, not jammed together
        assert!(
            !text.contains("message.This"),
            "Parts should be separated, not concatenated directly: {text}"
        );
    }

    #[test]
    fn parse_multiple_html_body_parts_concatenated() {
        // AC-1: Same as above but for htmlBody array.
        let item = serde_json::json!({
            "id": "email-6",
            "blobId": "blob-6",
            "threadId": "thread-6",
            "mailboxIds": {},
            "keywords": {},
            "receivedAt": "2024-01-15T10:30:00Z",
            "subject": "Fwd: Newsletter",
            "from": [{"email": "alice@example.com"}],
            "to": [{"email": "bob@example.com"}],
            "cc": [],
            "preview": "FYI",
            "hasAttachment": false,
            "size": 5000,
            "textBody": [],
            "htmlBody": [
                {"partId": "1", "type": "text/html"},
                {"partId": "2", "type": "text/html"}
            ],
            "bodyValues": {
                "1": {"value": "<p>FYI see below</p>"},
                "2": {"value": "<div>Original newsletter content</div>"}
            },
            "bodyStructure": {"type": "multipart/mixed"}
        });
        let email = parse_jmap_email(&item, true);
        let html = email.html_body.expect("html_body should be Some");
        assert!(
            html.contains("<p>FYI see below</p>"),
            "Should contain first HTML part: {html}"
        );
        assert!(
            html.contains("<div>Original newsletter content</div>"),
            "Should contain second HTML part: {html}"
        );
        // Finding #3: HTML parts should be separated with a newline
        assert!(
            html.contains("</p>\n<div>"),
            "HTML parts should be separated by newline: {html}"
        );
    }

    // --- build_draft_email html_body tests (THE-153) ---

    #[test]
    fn draft_text_only_when_no_html_body() {
        // AC-6: Regression — existing text-only behavior unchanged
        let sub = simple_submission();
        let draft = build_draft_email(&sub, "alice@example.com", "mb-drafts");
        // RFC 8621: textBody/htmlBody must NOT appear when bodyStructure is set
        assert!(
            !draft.contains_key("textBody"),
            "textBody must not be set when bodyStructure is present"
        );
        assert_eq!(draft["bodyValues"]["body"]["value"], "Hello");
        assert!(
            draft.contains_key("bodyStructure"),
            "Text-only draft should have bodyStructure"
        );
        assert_eq!(
            draft["bodyStructure"]["type"], "text/plain",
            "Text-only bodyStructure should be text/plain"
        );
    }

    #[test]
    fn draft_multipart_when_html_body_present() {
        // AC-5: When html_body is Some, draft should include both
        // text/plain and text/html parts (multipart/alternative).
        let sub = EmailSubmission {
            to: vec!["bob@example.com".into()],
            cc: vec![],
            subject: "Rich email".into(),
            text_body: "Hello, world!".into(),
            bcc: None,
            html_body: Some("<p>Hello, world!</p>".into()),
            in_reply_to: None,
            references: None,
            attachments: vec![],
            calendar_ics: None,
        };
        let draft = build_draft_email(&sub, "alice@example.com", "mb-drafts");
        // RFC 8621: textBody/htmlBody must NOT appear when bodyStructure is set
        assert!(
            !draft.contains_key("textBody"),
            "textBody must not be set when bodyStructure is present"
        );
        assert!(
            !draft.contains_key("htmlBody"),
            "htmlBody must not be set when bodyStructure is present"
        );
        // bodyValues should contain the HTML content
        let body_values = draft["bodyValues"]
            .as_object()
            .expect("bodyValues should be an object");
        let has_html = body_values
            .values()
            .any(|v| v["value"].as_str() == Some("<p>Hello, world!</p>"));
        assert!(has_html, "bodyValues should contain the HTML content");
        // Should still have text body too
        let has_text = body_values
            .values()
            .any(|v| v["value"].as_str() == Some("Hello, world!"));
        assert!(has_text, "bodyValues should still contain the text content");
        // bodyStructure with multipart/alternative
        assert!(
            draft.contains_key("bodyStructure"),
            "Multipart draft should have bodyStructure"
        );
        assert_eq!(
            draft["bodyStructure"]["type"], "multipart/alternative",
            "bodyStructure type should be multipart/alternative"
        );
        let sub_parts = draft["bodyStructure"]["subParts"]
            .as_array()
            .expect("bodyStructure should have subParts array");
        assert_eq!(sub_parts.len(), 2, "Should have text and html sub-parts");
        assert_eq!(sub_parts[0]["type"], "text/plain");
        assert_eq!(sub_parts[1]["type"], "text/html");
    }

    // --- uuid_v4 tests ---

    #[test]
    fn uuid_v4_format() {
        let id = uuid_v4();
        // 8-4-4-4-12 hex format
        let parts: Vec<&str> = id.split('-').collect();
        assert_eq!(parts.len(), 5, "UUID should have 5 parts: {id}");
        assert_eq!(parts[0].len(), 8);
        assert_eq!(parts[1].len(), 4);
        assert_eq!(parts[2].len(), 4);
        assert_eq!(parts[3].len(), 4);
        assert_eq!(parts[4].len(), 12);
        // All hex chars
        assert!(
            id.chars().all(|c| c.is_ascii_hexdigit() || c == '-'),
            "UUID should be hex: {id}"
        );
    }

    #[test]
    fn uuid_v4_version_bits() {
        let id = uuid_v4();
        // Third group should start with '4' (version 4)
        let third = id.split('-').nth(2).unwrap();
        assert!(
            third.starts_with('4'),
            "Version nibble should be 4: {third}"
        );
    }

    #[test]
    fn uuid_v4_variant_bits() {
        let id = uuid_v4();
        // Fourth group first char should be 8, 9, a, or b (variant 10xx)
        let fourth = id.split('-').nth(3).unwrap();
        let first_char = fourth.chars().next().unwrap();
        assert!(
            matches!(first_char, '8' | '9' | 'a' | 'b'),
            "Variant nibble should be 8/9/a/b: {first_char}"
        );
    }

    #[test]
    fn uuid_v4_unique() {
        let a = uuid_v4();
        let b = uuid_v4();
        assert_ne!(a, b, "Two UUIDs should not be identical");
    }

    // --- build_itip_mime tests (kata vt0m) ---

    #[test]
    fn itip_mime_ascii_subject_passes_through_and_non_ascii_is_rfc2047() {
        let mut sub = itip_submission();
        sub.subject = "Re: Team Standup".into();
        let mime = build_itip_mime(&sub, "bob@example.com", &[]);
        assert!(mime.contains("Subject: Re: Team Standup\r\n"));

        let mime = build_itip_mime(&itip_submission(), "bob@example.com", &[]);
        let subject_line = mime
            .lines()
            .find(|l| l.starts_with("Subject: "))
            .expect("Subject header present");
        assert!(
            subject_line.contains("=?UTF-8?B?"),
            "non-ASCII subject must be RFC 2047 encoded: {subject_line}"
        );
        assert!(
            !subject_line.contains('é'),
            "no raw non-ASCII may appear in a header: {subject_line}"
        );
    }

    #[test]
    fn itip_mime_long_non_ascii_subject_folds_into_short_encoded_words() {
        let mut sub = itip_submission();
        sub.subject = "Re: Réunion très importante de l'équipe européenne à Genève".into();
        let mime = build_itip_mime(&sub, "bob@example.com", &[]);
        let subject_start = mime.find("Subject: ").unwrap() + "Subject: ".len();
        let subject_end = mime[subject_start..]
            .find("\r\nMIME-Version")
            .map(|i| subject_start + i)
            .unwrap();
        for word in mime[subject_start..subject_end].split("\r\n ") {
            assert!(
                word.len() <= 75,
                "each RFC 2047 encoded-word must be ≤75 chars: {word:?} ({})",
                word.len()
            );
            assert!(word.starts_with("=?UTF-8?B?") && word.ends_with("?="));
        }
    }

    #[test]
    fn itip_mime_strips_crlf_header_injection() {
        let mut sub = itip_submission();
        sub.subject = "Re: x\r\nBcc: evil@example.com".into();
        let mime = build_itip_mime(&sub, "bob@example.com", &[]);
        assert!(
            !mime.lines().any(|l| l.starts_with("Bcc:")),
            "CRLF in a subject must not inject a header line:\n{mime}"
        );
    }

    #[test]
    fn itip_mime_base64_lines_stay_within_rfc2045_limit() {
        let mut sub = itip_submission();
        sub.calendar_ics = Some(format!(
            "BEGIN:VCALENDAR\r\nMETHOD:REPLY\r\nDESCRIPTION:{}\r\nEND:VCALENDAR\r\n",
            "x".repeat(600)
        ));
        let mime = build_itip_mime(&sub, "bob@example.com", &[]);
        for line in mime.lines() {
            assert!(
                line.len() <= 78,
                "MIME line exceeds RFC 2045 length limit ({}): {line:?}",
                line.len()
            );
        }
    }

    #[test]
    fn itip_mime_text_part_decodes_to_text_body() {
        use base64::Engine;
        let sub = itip_submission();
        let mime = build_itip_mime(&sub, "bob@example.com", &[]);
        let text_start = mime.find("Content-Type: text/plain").unwrap();
        let b64_start = mime[text_start..].find("\r\n\r\n").unwrap() + text_start + 4;
        let b64_end = mime[b64_start..].find("\r\n--").unwrap() + b64_start;
        let b64: String = mime[b64_start..b64_end]
            .chars()
            .filter(|c| !c.is_ascii_whitespace())
            .collect();
        let text = String::from_utf8(
            base64::engine::general_purpose::STANDARD
                .decode(b64)
                .unwrap(),
        )
        .unwrap();
        assert_eq!(text, sub.text_body);
    }

    #[test]
    fn draft_without_calendar_ics_unchanged() {
        let sub = simple_submission();
        let draft = build_draft_email(&sub, "alice@example.com", "mb-drafts");
        assert_eq!(
            draft["bodyStructure"]["type"], "text/plain",
            "Non-calendar draft should stay text/plain"
        );
    }

    // --- iTIP reply via Email/import: loopback behavioral tests (kata vt0m) ---
    //
    // Empirically verified against live Fastmail (2026-07-29): Email/set
    // bodyStructure CANNOT put `method=REPLY` on the wire — `type` with
    // parameters and `header:Content-Type` are both rejected as
    // invalidProperties (the two production failures), a blob's upload
    // Content-Type is silently replaced by one generated from the part
    // properties, and `charset` is not a settable part property. The only
    // channel that preserves `Content-Type: text/calendar; method=REPLY` is
    // building the RFC822 message client-side, uploading it as
    // `message/rfc822`, and creating the email via Email/import. These tests
    // drive the real `send_email` against a loopback recorder and assert on
    // what actually goes over the wire.

    fn itip_submission() -> EmailSubmission {
        EmailSubmission {
            to: vec!["organizer@example.com".into()],
            cc: vec![],
            subject: "Re: Réunion d'équipe".into(),
            text_body: "bob@example.com has accepted the invitation: Réunion d'équipe".into(),
            bcc: None,
            html_body: None,
            in_reply_to: None,
            references: None,
            attachments: vec![],
            calendar_ics: Some(
                "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nMETHOD:REPLY\r\nBEGIN:VEVENT\r\n\
                 SUMMARY:Réunion d'équipe\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n"
                    .into(),
            ),
        }
    }

    /// Loopback recorder + a connected-looking session pointed at it, with
    /// Drafts/Sent cached. The canned response body is returned for every
    /// request (upload and JMAP alike).
    async fn loopback_session(
        canned: serde_json::Value,
    ) -> (
        JmapSession,
        std::sync::Arc<std::sync::Mutex<Vec<caldav_recorder::RecordedRequest>>>,
    ) {
        let (base, recorded) =
            caldav_recorder::spawn(axum::http::StatusCode::OK, canned.to_string().into_bytes())
                .await;
        let mut s = JmapSession::new("bob@example.com", "test-token", None);
        s.api_url = Some(format!("{base}/jmap"));
        s.upload_url = Some(format!("{base}/upload/{{accountId}}"));
        s.account_id = Some("acc1".into());
        s.identity_id = Some("ident1".into());
        for (id, name, role) in [
            ("mb-drafts", "Drafts", "drafts"),
            ("mb-sent", "Sent", "sent"),
        ] {
            s.mailbox_cache.insert(
                id.into(),
                Mailbox {
                    id: id.into(),
                    name: name.into(),
                    role: Some(role.into()),
                    total_emails: 0,
                    unread_emails: 0,
                    parent_id: None,
                },
            );
        }
        (s, recorded)
    }

    /// `loopback_session` canned for the happy-path import flow: the one
    /// body satisfies both parsers that see it — `upload_blob` reads
    /// `blobId`/`size`, `jmap_call` reads `methodResponses`.
    async fn spawn_jmap_loopback() -> (
        JmapSession,
        std::sync::Arc<std::sync::Mutex<Vec<caldav_recorder::RecordedRequest>>>,
    ) {
        loopback_session(serde_json::json!({
            "blobId": "blob-rfc822-1",
            "size": 1,
            "methodResponses": [
                ["Email/import",
                 {"created": {"e": {"id": "E-imported-1", "blobId": "blob-rfc822-1"}}},
                 "0"],
                ["EmailSubmission/set",
                 {"created": {"send": {"id": "S1", "emailId": "E-imported-1"}}},
                 "1"]
            ]
        }))
        .await
    }

    /// Walk a JSON value and assert it contains none of the two shapes
    /// Fastmail rejected in production: an object key starting with
    /// `header:` (rejected `bodyStructure/subParts[1]/header:Content-Type`)
    /// or a `type` value carrying a `;`-parameter (rejected
    /// `bodyStructure/subParts[1]/type`).
    fn assert_no_rejected_part_shapes(v: &serde_json::Value) {
        match v {
            serde_json::Value::Object(map) => {
                for (k, val) in map {
                    assert!(
                        !k.starts_with("header:"),
                        "Email/set part-input schema has no header:* properties — \
                         Fastmail rejects them as invalidProperties (kata vt0m): {k}"
                    );
                    if k == "type"
                        && let Some(t) = val.as_str()
                    {
                        assert!(
                            !t.contains(';'),
                            "JMAP `type` is a bare MIME token; `{t}` carries a parameter \
                             and is rejected as invalidProperties (kata vt0m)"
                        );
                    }
                    assert_no_rejected_part_shapes(val);
                }
            }
            serde_json::Value::Array(items) => {
                for item in items {
                    assert_no_rejected_part_shapes(item);
                }
            }
            _ => {}
        }
    }

    #[tokio::test]
    async fn itip_reply_sends_no_shape_fastmail_rejects() {
        // Regression signatures from BOTH production failures: the original
        // `"type": "text/calendar; method=REPLY"` and the first fix's
        // `"header:Content-Type"` — every JSON body sent to the JMAP API
        // endpoint must be free of both.
        let (mut s, recorded) = spawn_jmap_loopback().await;
        let sub = itip_submission();
        let result = send_email(&mut s, &sub, "bob@example.com", Some("ident1")).await;
        assert!(result.is_ok(), "send should succeed: {result:?}");
        let reqs = recorded.lock().unwrap().clone();
        let jmap_bodies: Vec<serde_json::Value> = reqs
            .iter()
            .filter(|r| r.path.ends_with("/jmap"))
            .map(|r| serde_json::from_slice(&r.body).expect("JMAP body is JSON"))
            .collect();
        assert!(!jmap_bodies.is_empty(), "expected at least one JMAP call");
        for body in &jmap_bodies {
            assert_no_rejected_part_shapes(body);
        }
    }

    #[tokio::test]
    async fn itip_reply_uploads_rfc822_whose_calendar_part_carries_method_reply() {
        // The delivery channel: the calendar part's Content-Type — with
        // method=REPLY and charset=utf-8 — must be inside a client-built
        // RFC822 message uploaded as message/rfc822 (the only channel
        // Fastmail preserves, verified live 2026-07-29).
        let (mut s, recorded) = spawn_jmap_loopback().await;
        let sub = itip_submission();
        let result = send_email(&mut s, &sub, "bob@example.com", Some("ident1")).await;
        assert!(result.is_ok(), "send should succeed: {result:?}");
        let reqs = recorded.lock().unwrap().clone();
        let uploads: Vec<_> = reqs
            .iter()
            .filter(|r| r.path.starts_with("/upload/"))
            .collect();
        assert_eq!(
            uploads.len(),
            1,
            "iTIP reply must upload exactly one message/rfc822 blob"
        );
        let up = uploads[0];
        assert_eq!(up.path, "/upload/acc1", "upload must substitute accountId");
        assert_eq!(
            up.content_type.as_deref(),
            Some("message/rfc822"),
            "blob must be uploaded as message/rfc822 so Email/import accepts it"
        );
        let mime = String::from_utf8(up.body.clone()).expect("MIME is UTF-8");
        assert!(
            mime.contains("Content-Type: text/calendar; method=REPLY; charset=utf-8"),
            "calendar part must carry method=REPLY (RFC 5546 §3.2) and charset=utf-8 \
             (RFC 5545 §8.1) on its MIME Content-Type; got:\n{mime}"
        );
        // The ICS travels base64-encoded (8-bit safe, and no line of base64
        // output can collide with a boundary delimiter). Decode the calendar
        // part and confirm the actual ICS arrived intact, non-ASCII included.
        let cal_start = mime
            .find("Content-Type: text/calendar")
            .expect("calendar part present");
        let b64_start = mime[cal_start..]
            .find("\r\n\r\n")
            .map(|i| cal_start + i + 4)
            .expect("calendar part has a header/body separator");
        let b64_end = mime[b64_start..]
            .find("\r\n--")
            .map(|i| b64_start + i)
            .expect("calendar part is terminated by a boundary");
        let b64: String = mime[b64_start..b64_end]
            .chars()
            .filter(|c| !c.is_ascii_whitespace())
            .collect();
        use base64::Engine;
        let ics = String::from_utf8(
            base64::engine::general_purpose::STANDARD
                .decode(b64)
                .expect("calendar part body is valid base64"),
        )
        .expect("decoded ICS is UTF-8");
        assert_eq!(
            ics,
            sub.calendar_ics.as_deref().unwrap(),
            "decoded calendar part must be the ICS byte-for-byte"
        );
        assert!(ics.contains("METHOD:REPLY"));
        assert!(ics.contains("Réunion"), "non-ASCII must survive encoding");
    }

    #[tokio::test]
    async fn itip_reply_imports_then_submits_with_creation_id_reference() {
        // The verified JMAP chain: one request — Email/import into Drafts
        // ($draft), then EmailSubmission/set referencing the import via the
        // "#e" creation id, moving the message to Sent on success (the same
        // Drafts→Sent patch the plain send path uses).
        let (mut s, recorded) = spawn_jmap_loopback().await;
        let sub = itip_submission();
        let result = send_email(&mut s, &sub, "bob@example.com", Some("ident1")).await;
        assert_eq!(
            result.expect("send should succeed").as_deref(),
            Some("E-imported-1"),
            "send_email must return the sent email's id"
        );
        let reqs = recorded.lock().unwrap().clone();
        let jmap_bodies: Vec<serde_json::Value> = reqs
            .iter()
            .filter(|r| r.path.ends_with("/jmap"))
            .map(|r| serde_json::from_slice(&r.body).expect("JMAP body is JSON"))
            .collect();
        assert_eq!(jmap_bodies.len(), 1, "import + submit must be one request");
        let calls = &jmap_bodies[0]["methodCalls"];
        assert_eq!(calls[0][0], "Email/import");
        let e = &calls[0][1]["emails"]["e"];
        assert_eq!(e["blobId"], "blob-rfc822-1");
        assert_eq!(e["mailboxIds"]["mb-drafts"], true);
        assert_eq!(e["keywords"]["$draft"], true);
        assert_eq!(calls[1][0], "EmailSubmission/set");
        let send = &calls[1][1]["create"]["send"];
        assert_eq!(
            send["emailId"], "#e",
            "submission must reference the import by creation id"
        );
        assert_eq!(send["identityId"], "ident1");
        assert_eq!(send["envelope"]["mailFrom"]["email"], "bob@example.com");
        assert_eq!(
            send["envelope"]["rcptTo"][0]["email"],
            "organizer@example.com"
        );
        let patch = &calls[1][1]["onSuccessUpdateEmail"]["#send"];
        assert!(
            patch["mailboxIds/mb-drafts"].is_null()
                && patch
                    .as_object()
                    .unwrap()
                    .contains_key("mailboxIds/mb-drafts"),
            "success patch must remove the message from Drafts"
        );
        assert_eq!(patch["mailboxIds/mb-sent"], true);
        assert!(
            patch["keywords/$draft"].is_null()
                && patch.as_object().unwrap().contains_key("keywords/$draft"),
            "success patch must clear $draft"
        );
    }

    /// Live acceptance gate (kata vt0m): run the REAL `send_email` iTIP path
    /// against the REAL Fastmail server and verify the reply arrives with
    /// `Content-Type: text/calendar; method=REPLY; charset=utf-8` on the
    /// calendar part. This is the test the first vt0m fix lacked — its
    /// loopback tests asserted the client's output shape, which went green
    /// on a shape Fastmail rejects. A recorder can't catch a server
    /// rejection it doesn't model; only the live server is authoritative.
    ///
    /// `#[ignore]` because it needs the local Fastmail config and sends a
    /// real message (to the account itself; both copies are destroyed at
    /// the end). Run with:
    /// `cargo test --lib live_fastmail_itip_reply_roundtrip -- --ignored --nocapture`
    #[tokio::test]
    #[ignore]
    async fn live_fastmail_itip_reply_roundtrip() {
        use base64::Engine;

        // Load the real Fastmail credentials the way main() does.
        let config_path = crate::platform::config_dir().join("supervillain/config");
        let (cfg, _) = crate::accounts::parse_config(&config_path);
        let (username, api_token) = cfg
            .accounts
            .values()
            .find_map(|a| match a {
                crate::accounts::AccountConfig::Fastmail {
                    username,
                    api_token,
                    ..
                } => Some((username.clone(), api_token.clone())),
                _ => None,
            })
            .expect("live test needs a Fastmail account in the local config");

        let mut s = JmapSession::new(&username, &api_token, None);
        connect(&mut s).await.expect("connect to Fastmail");
        for mb in get_mailboxes(&s).await.expect("fetch mailboxes") {
            s.mailbox_cache.insert(mb.id.clone(), mb);
        }
        let account_id = s.account_id.clone().unwrap();
        let inbox_id = s
            .mailbox_cache
            .values()
            .find(|mb| mb.role.as_deref() == Some("inbox"))
            .expect("inbox")
            .id
            .clone();

        // Unique marker so polling can't match an earlier run's message.
        let marker = format!(
            "vt0m-live-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_millis()
        );

        // The real generator, a non-ASCII summary (the roborev 385 charset
        // concern), replying to self so no third party sees the probe.
        let event = CalendarEvent {
            uid: format!("{marker}@supervillain"),
            summary: format!("Réunion d'équipe {marker}"),
            dtstart: chrono::Utc::now() + chrono::Duration::days(1),
            dtend: None,
            location: None,
            description: None,
            organizer_email: username.clone(),
            organizer_name: None,
            attendees: vec![],
            sequence: 0,
            method: "REQUEST".into(),
            raw_ics: String::new(),
            user_rsvp_status: None,
            is_update: false,
        };
        let rsvp_ics = crate::calendar::generate_rsvp_with_tz(
            &event,
            &username,
            &crate::types::RsvpStatus::Accepted,
            chrono_tz::America::Chicago,
        );
        let sub = EmailSubmission {
            to: vec![username.clone()],
            cc: vec![],
            subject: format!("Re: {}", event.summary),
            text_body: format!("{username} has accepted the invitation: {}", event.summary),
            bcc: None,
            html_body: None,
            in_reply_to: None,
            references: None,
            attachments: vec![],
            calendar_ics: Some(rsvp_ics.clone()),
        };

        let sent_id = send_email(&mut s, &sub, &username, None)
            .await
            .expect("live send_email must be accepted by Fastmail")
            .expect("send_email returns the email id");

        // Poll the inbox for arrival.
        let mut arrived_id = None;
        for _ in 0..30 {
            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
            let resp = jmap_call(
                &s,
                vec![serde_json::json!([
                    "Email/query",
                    {
                        "accountId": &account_id,
                        "filter": { "inMailbox": &inbox_id, "subject": &marker }
                    },
                    "0"
                ])],
            )
            .await
            .expect("Email/query");
            if let Some(id) = resp["methodResponses"][0][1]["ids"][0].as_str() {
                arrived_id = Some(id.to_string());
                break;
            }
        }
        let arrived_id = arrived_id.expect("iTIP reply never arrived in the inbox");

        // Download the raw message and verify what's actually on the wire.
        let resp = jmap_call(
            &s,
            vec![serde_json::json!([
                "Email/get",
                { "accountId": &account_id, "ids": [&arrived_id], "properties": ["blobId"] },
                "0"
            ])],
        )
        .await
        .expect("Email/get");
        let blob_id = resp["methodResponses"][0][1]["list"][0]["blobId"]
            .as_str()
            .expect("blobId")
            .to_string();
        let (_, raw) = download_blob(&s, &blob_id, "probe.eml")
            .await
            .expect("download raw");
        let mime = String::from_utf8_lossy(&raw);

        assert!(
            mime.contains("Content-Type: text/calendar; method=REPLY; charset=utf-8"),
            "arrived message must carry method=REPLY on the calendar part:\n{mime}"
        );
        assert!(
            mime.lines().any(|l| l.starts_with("Date:")),
            "Fastmail's MTA must have added a Date header"
        );
        assert!(
            mime.to_lowercase().contains("message-id:"),
            "Fastmail's MTA must have added a Message-Id header"
        );
        // Decode the calendar part; the generated ICS must arrive intact.
        let cal_start = mime.find("Content-Type: text/calendar").unwrap();
        let b64_start = mime[cal_start..].find("\r\n\r\n").unwrap() + cal_start + 4;
        let b64_end = mime[b64_start..].find("\r\n--").unwrap() + b64_start;
        let b64: String = mime[b64_start..b64_end]
            .chars()
            .filter(|c| !c.is_ascii_whitespace())
            .collect();
        let ics = String::from_utf8(
            base64::engine::general_purpose::STANDARD
                .decode(b64)
                .expect("valid base64"),
        )
        .expect("UTF-8 ICS");
        assert_eq!(ics, rsvp_ics, "ICS must arrive byte-for-byte");
        assert!(ics.contains("METHOD:REPLY"));
        assert!(ics.contains("PARTSTAT=ACCEPTED"));
        assert!(ics.contains("Réunion"), "non-ASCII summary must survive");

        // Cleanup: destroy both the received copy and the Sent copy.
        let destroyed = jmap_call(
            &s,
            vec![serde_json::json!([
                "Email/set",
                { "accountId": &account_id, "destroy": [&arrived_id, &sent_id] },
                "0"
            ])],
        )
        .await
        .expect("cleanup destroy");
        println!(
            "live round-trip OK; destroyed: {}",
            destroyed["methodResponses"][0][1]["destroyed"]
        );
        println!(
            "--- arrived calendar part Content-Type verified: text/calendar; method=REPLY; charset=utf-8 ---"
        );
    }

    /// Live acceptance gate (kata 2xh9): run the REAL `send_email` invite
    /// path against the REAL Fastmail server and verify the invite arrives
    /// with `Content-Type: text/calendar; method=REQUEST; charset=utf-8` on
    /// the calendar part. Mirrors `live_fastmail_itip_reply_roundtrip` (kata
    /// vt0m) — the lesson there was that loopback tests asserting the
    /// client's output shape go green on a shape Fastmail rejects; only the
    /// live server is authoritative. Invites route through the same
    /// Email/import path as replies because Email/set cannot carry the MIME
    /// `method` parameter (kata vt0m for REPLY, kata 2xh9 for REQUEST).
    ///
    /// `#[ignore]` because it needs the local Fastmail config and sends a
    /// real message (to the account itself; both copies are destroyed at the
    /// end). Run with:
    /// `cargo test --lib live_fastmail_invite_roundtrip -- --ignored --nocapture`
    #[tokio::test]
    #[ignore]
    async fn live_fastmail_invite_roundtrip() {
        use base64::Engine;

        let config_path = crate::platform::config_dir().join("supervillain/config");
        let (cfg, _) = crate::accounts::parse_config(&config_path);
        let (username, api_token) = cfg
            .accounts
            .values()
            .find_map(|a| match a {
                crate::accounts::AccountConfig::Fastmail {
                    username,
                    api_token,
                    ..
                } => Some((username.clone(), api_token.clone())),
                _ => None,
            })
            .expect("live test needs a Fastmail account in the local config");

        let mut s = JmapSession::new(&username, &api_token, None);
        connect(&mut s).await.expect("connect to Fastmail");
        for mb in get_mailboxes(&s).await.expect("fetch mailboxes") {
            s.mailbox_cache.insert(mb.id.clone(), mb);
        }
        let account_id = s.account_id.clone().unwrap();
        let inbox_id = s
            .mailbox_cache
            .values()
            .find(|mb| mb.role.as_deref() == Some("inbox"))
            .expect("inbox")
            .id
            .clone();

        // Unique marker so polling can't match an earlier run's message.
        let marker = format!(
            "2xh9-live-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_millis()
        );

        // A non-ASCII invite (charset concern from kata vt0m roborev 385),
        // sent to self so no third party sees the probe.
        let tz = chrono_tz::America::Chicago;
        let dtstart = (chrono::Utc::now() + chrono::Duration::days(1)).with_timezone(&tz);
        let dtend = dtstart + chrono::Duration::hours(1);
        let summary = format!("Réunion d'équipe {marker}");
        let ics = crate::calendar::generate_invite(
            &username,
            None,
            &summary,
            None,
            None,
            dtstart,
            dtend,
            &[crate::types::Attendee {
                email: username.clone(),
                name: None,
                status: "NEEDS-ACTION".into(),
            }],
            Some(&format!("{marker}@supervillain")),
        );
        let sub = EmailSubmission {
            to: vec![username.clone()],
            cc: vec![],
            subject: format!("Invitation: {summary}"),
            text_body: format!("{username} invites you to: {summary}"),
            bcc: None,
            html_body: None,
            in_reply_to: None,
            references: None,
            attachments: vec![],
            calendar_ics: Some(ics.clone()),
        };

        let sent_id = send_email(&mut s, &sub, &username, None)
            .await
            .expect("live send_email must be accepted by Fastmail")
            .expect("send_email returns the email id");

        // Poll the inbox for arrival.
        let mut arrived_id = None;
        for _ in 0..30 {
            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
            let resp = jmap_call(
                &s,
                vec![serde_json::json!([
                    "Email/query",
                    {
                        "accountId": &account_id,
                        "filter": { "inMailbox": &inbox_id, "subject": &marker }
                    },
                    "0"
                ])],
            )
            .await
            .expect("Email/query");
            if let Some(id) = resp["methodResponses"][0][1]["ids"][0].as_str() {
                arrived_id = Some(id.to_string());
                break;
            }
        }
        let arrived_id = arrived_id.expect("invite never arrived in the inbox");

        // Download the raw message and verify what's actually on the wire.
        let resp = jmap_call(
            &s,
            vec![serde_json::json!([
                "Email/get",
                { "accountId": &account_id, "ids": [&arrived_id], "properties": ["blobId"] },
                "0"
            ])],
        )
        .await
        .expect("Email/get");
        let blob_id = resp["methodResponses"][0][1]["list"][0]["blobId"]
            .as_str()
            .expect("blobId")
            .to_string();
        let (_, raw) = download_blob(&s, &blob_id, "probe.eml")
            .await
            .expect("download raw");
        let mime = String::from_utf8_lossy(&raw);

        assert!(
            mime.contains("Content-Type: text/calendar; method=REQUEST; charset=utf-8"),
            "arrived message must carry method=REQUEST on the calendar part:\n{mime}"
        );
        assert!(
            mime.lines().any(|l| l.starts_with("Date:")),
            "Fastmail's MTA must have added a Date header"
        );
        assert!(
            mime.to_lowercase().contains("message-id:"),
            "Fastmail's MTA must have added a Message-Id header"
        );
        // Decode the calendar part; the generated ICS must arrive intact.
        let cal_start = mime.find("Content-Type: text/calendar").unwrap();
        let b64_start = mime[cal_start..].find("\r\n\r\n").unwrap() + cal_start + 4;
        let b64_end = mime[b64_start..].find("\r\n--").unwrap() + b64_start;
        let b64: String = mime[b64_start..b64_end]
            .chars()
            .filter(|c| !c.is_ascii_whitespace())
            .collect();
        let arrived_ics = String::from_utf8(
            base64::engine::general_purpose::STANDARD
                .decode(b64)
                .expect("valid base64"),
        )
        .expect("UTF-8 ICS");
        assert_eq!(arrived_ics, ics, "ICS must arrive byte-for-byte");
        assert!(arrived_ics.contains("METHOD:REQUEST"));
        assert!(
            arrived_ics.contains("Réunion"),
            "non-ASCII summary must survive"
        );

        // Cleanup: destroy both the received copy and the Sent copy.
        let destroyed = jmap_call(
            &s,
            vec![serde_json::json!([
                "Email/set",
                { "accountId": &account_id, "destroy": [&arrived_id, &sent_id] },
                "0"
            ])],
        )
        .await
        .expect("cleanup destroy");
        println!(
            "live invite round-trip OK; destroyed: {}",
            destroyed["methodResponses"][0][1]["destroyed"]
        );
        println!(
            "--- arrived calendar part Content-Type verified: text/calendar; method=REQUEST; charset=utf-8 ---"
        );
    }

    #[test]
    #[should_panic(expected = "calendar_ics routes through send_itip_via_import")]
    fn build_draft_email_rejects_calendar_ics() {
        // kata 2xh9: all calendar_ics (REQUEST and REPLY) route through the
        // Email/import path (send_itip_via_import) because Email/set cannot
        // carry the MIME `method` parameter (kata vt0m). build_draft_email is
        // Email/set-only and must never see calendar_ics — reaching it is a
        // routing bug that would send a mislabeled message to a third party.
        build_draft_email(&invite_submission(), "bob@example.com", "mb-drafts");
    }

    #[tokio::test]
    async fn calendar_ics_with_html_body_is_rejected() {
        // The import path builds a text + calendar MIME and does not emit
        // html_body, so calendar_ics + html_body is rejected — preserving
        // the old build_draft_email mutual-exclusion invariant at the
        // routing layer (the producer contract is enforced where it's built).
        let (mut s, _recorded) = spawn_jmap_loopback().await;
        let mut sub = invite_submission();
        sub.attachments = vec![];
        sub.html_body = Some("<p>should not coexist with calendar</p>".into());
        let err = send_email(&mut s, &sub, "bob@example.com", Some("ident1"))
            .await
            .expect_err("calendar_ics + html_body must be rejected");
        assert!(
            format!("{err}").contains("html_body"),
            "error must name html_body: {err}"
        );
    }

    // --- invite path: METHOD:REQUEST via Email/import (kata 2xh9) ---
    //
    // kata vt0m proved Email/set bodyStructure CANNOT put a MIME `method`
    // parameter on the wire (bare `type` drops it; `type` with params,
    // `header:Content-Type`, and a blob's upload Content-Type are all
    // rejected or silently replaced). The reply fix routed METHOD:REPLY
    // through a client-built RFC822 + Email/import. kata 2xh9 extends the
    // same route to METHOD:REQUEST invites: without `method=REQUEST` on the
    // calendar part's Content-Type the message is just a calendar
    // attachment, not an iTIP REQUEST (RFC 5546 §3.2), and a strict
    // recipient client won't surface RSVP buttons or auto-file it.

    /// An invite as `send_invite_handler` builds it: METHOD:REQUEST ICS plus
    /// cc and attachments from the request body.
    fn invite_submission() -> EmailSubmission {
        EmailSubmission {
            to: vec!["guest@example.com".into()],
            cc: vec!["observer@example.com".into()],
            subject: "Team Standup".into(),
            text_body: "You're invited".into(),
            bcc: None,
            html_body: None,
            in_reply_to: None,
            references: None,
            attachments: vec![pdf_attachment()],
            calendar_ics: Some(
                "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nMETHOD:REQUEST\r\nBEGIN:VEVENT\r\n\
                 SUMMARY:Team Standup\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n"
                    .into(),
            ),
        }
    }

    #[tokio::test]
    async fn invite_goes_through_import_path_not_email_set() {
        // The routing change: an invite (METHOD:REQUEST) uploads a
        // message/rfc822 blob and creates the email via Email/import — NOT
        // Email/set (which cannot carry method=REQUEST).
        let (mut s, recorded) = spawn_jmap_loopback().await;
        let mut sub = invite_submission();
        sub.attachments = vec![];
        let result = send_email(&mut s, &sub, "bob@example.com", Some("ident1")).await;
        assert!(result.is_ok(), "invite send must succeed: {result:?}");

        let reqs = recorded.lock().unwrap().clone();
        let uploads: Vec<_> = reqs
            .iter()
            .filter(|r| r.path.starts_with("/upload/"))
            .collect();
        assert_eq!(
            uploads.len(),
            1,
            "invite must upload exactly one message/rfc822 blob"
        );
        assert_eq!(uploads[0].content_type.as_deref(), Some("message/rfc822"));
        let body: serde_json::Value = serde_json::from_slice(
            &reqs
                .iter()
                .find(|r| r.path.ends_with("/jmap"))
                .expect("one JMAP call")
                .body,
        )
        .unwrap();
        let calls = &body["methodCalls"];
        assert_eq!(
            calls[0][0], "Email/import",
            "invite must use Email/import, not Email/set"
        );
        assert!(
            calls
                .as_array()
                .expect("methodCalls is an array")
                .iter()
                .all(|c| c[0] != "Email/set"),
            "no Email/set call for an invite: {calls:?}"
        );
    }

    #[tokio::test]
    async fn invite_upload_carries_method_request_and_ics_byte_identical() {
        // The whole point of the reroute: the calendar part's Content-Type
        // must carry method=REQUEST (RFC 5546 §3.2), and the ICS must arrive
        // byte-for-byte inside the uploaded RFC822.
        use base64::Engine;
        let (mut s, recorded) = spawn_jmap_loopback().await;
        let mut sub = invite_submission();
        sub.attachments = vec![];
        let ics = sub.calendar_ics.clone().unwrap();
        send_email(&mut s, &sub, "bob@example.com", Some("ident1"))
            .await
            .unwrap();

        let reqs = recorded.lock().unwrap().clone();
        let up = reqs
            .iter()
            .find(|r| r.path.starts_with("/upload/"))
            .expect("one upload");
        assert_eq!(up.content_type.as_deref(), Some("message/rfc822"));
        let mime = String::from_utf8(up.body.clone()).unwrap();
        assert!(
            mime.contains("Content-Type: text/calendar; method=REQUEST; charset=utf-8"),
            "calendar part must carry method=REQUEST on its MIME Content-Type:\n{mime}"
        );
        let cal_start = mime.find("Content-Type: text/calendar").unwrap();
        let b64_start = mime[cal_start..].find("\r\n\r\n").unwrap() + cal_start + 4;
        let b64_end = mime[b64_start..].find("\r\n--").unwrap() + b64_start;
        let b64: String = mime[b64_start..b64_end]
            .chars()
            .filter(|c| !c.is_ascii_whitespace())
            .collect();
        let decoded = String::from_utf8(
            base64::engine::general_purpose::STANDARD
                .decode(b64)
                .expect("calendar part is valid base64"),
        )
        .expect("decoded ICS is UTF-8");
        assert_eq!(
            decoded, ics,
            "ICS must be byte-identical in the calendar part"
        );
        assert!(decoded.contains("METHOD:REQUEST"));
    }

    #[tokio::test]
    async fn invite_upload_carries_cc_in_mime_and_envelope() {
        // cc rides on a `Cc:` header in the client-built MIME (it is NOT a
        // JMAP Email property on the import path) AND on the envelope rcptTo
        // so the cc recipient actually receives the invite.
        let (mut s, recorded) = spawn_jmap_loopback().await;
        let mut sub = invite_submission();
        sub.attachments = vec![];
        send_email(&mut s, &sub, "bob@example.com", Some("ident1"))
            .await
            .unwrap();

        let reqs = recorded.lock().unwrap().clone();
        let up = reqs
            .iter()
            .find(|r| r.path.starts_with("/upload/"))
            .expect("one upload");
        let mime = String::from_utf8(up.body.clone()).unwrap();
        assert!(
            mime.contains("Cc: observer@example.com"),
            "Cc header must be in the MIME: {mime}"
        );
        let body: serde_json::Value = serde_json::from_slice(
            &reqs
                .iter()
                .find(|r| r.path.ends_with("/jmap"))
                .expect("one JMAP call")
                .body,
        )
        .unwrap();
        let rcpt = body["methodCalls"][1][1]["create"]["send"]["envelope"]["rcptTo"]
            .as_array()
            .expect("envelope rcptTo");
        assert!(
            rcpt.iter().any(|r| r["email"] == "observer@example.com"),
            "envelope rcptTo must include the cc recipient: {rcpt:?}"
        );
    }

    #[tokio::test]
    async fn invite_import_inlines_attachments_byte_identical() {
        // Attachments are server-side blobs (from /api/upload); the import
        // path downloads each one and inlines it as a base64 part of the
        // client-built RFC822 (Email/import takes a single complete message,
        // so attachments cannot ride as separate blobId references). The
        // downloaded bytes must arrive byte-identical.
        use base64::Engine;
        let attachment_bytes = b"PDF-BYTES-12345".to_vec();
        let bytes_for_responder = attachment_bytes.clone();
        let (base, recorded) = caldav_recorder::spawn_scripted(move |_method, path| {
            let body = if path.starts_with("/download/") {
                bytes_for_responder.clone()
            } else if path.starts_with("/upload/") {
                serde_json::json!({"blobId":"blob-rfc822-1","size":1})
                    .to_string()
                    .into_bytes()
            } else {
                serde_json::json!({
                    "methodResponses": [
                        ["Email/import",
                         {"created": {"e": {"id": "E1", "blobId": "blob-rfc822-1"}}},
                         "0"],
                        ["EmailSubmission/set",
                         {"created": {"send": {"id": "S1", "emailId": "E1"}}},
                         "1"]
                    ]
                })
                .to_string()
                .into_bytes()
            };
            (axum::http::StatusCode::OK, body)
        })
        .await;
        let mut s = JmapSession::new("bob@example.com", "test-token", None);
        s.api_url = Some(format!("{base}/jmap"));
        s.upload_url = Some(format!("{base}/upload/{{accountId}}"));
        s.download_url = Some(format!("{base}/download/{{accountId}}/{{blobId}}/{{name}}"));
        s.account_id = Some("acc1".into());
        s.identity_id = Some("ident1".into());
        for (id, name, role) in [
            ("mb-drafts", "Drafts", "drafts"),
            ("mb-sent", "Sent", "sent"),
        ] {
            s.mailbox_cache.insert(
                id.into(),
                Mailbox {
                    id: id.into(),
                    name: name.into(),
                    role: Some(role.into()),
                    total_emails: 0,
                    unread_emails: 0,
                    parent_id: None,
                },
            );
        }
        let mut sub = invite_submission();
        sub.attachments = vec![Attachment {
            blob_id: "blob-pdf-123".into(),
            name: "report.pdf".into(),
            mime_type: "application/pdf".into(),
            size: attachment_bytes.len() as i64,
        }];
        send_email(&mut s, &sub, "bob@example.com", Some("ident1"))
            .await
            .unwrap();

        let reqs = recorded.lock().unwrap().clone();
        let downloads: Vec<_> = reqs
            .iter()
            .filter(|r| r.method == "GET" && r.path.starts_with("/download/"))
            .collect();
        assert_eq!(
            downloads.len(),
            1,
            "must download the attachment blob exactly once"
        );
        let uploads: Vec<_> = reqs
            .iter()
            .filter(|r| r.path.starts_with("/upload/"))
            .collect();
        assert_eq!(uploads.len(), 1, "must upload one message/rfc822");
        assert_eq!(uploads[0].content_type.as_deref(), Some("message/rfc822"));
        let mime = String::from_utf8(uploads[0].body.clone()).unwrap();
        assert!(
            mime.contains("Content-Type: application/pdf; name=\"report.pdf\""),
            "attachment part Content-Type with name: {mime}"
        );
        assert!(
            mime.contains("Content-Disposition: attachment; filename=\"report.pdf\""),
            "attachment part Content-Disposition: {mime}"
        );
        let disp = mime.find("Content-Disposition: attachment").unwrap();
        let b64_start = mime[disp..].find("\r\n\r\n").unwrap() + disp + 4;
        let b64_end = mime[b64_start..].find("\r\n--").unwrap() + b64_start;
        let b64: String = mime[b64_start..b64_end]
            .chars()
            .filter(|c| !c.is_ascii_whitespace())
            .collect();
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(b64)
            .expect("attachment part is valid base64");
        assert_eq!(
            decoded, attachment_bytes,
            "attachment bytes must be inlined byte-identical"
        );
    }

    #[tokio::test]
    async fn invite_import_chain_uses_email_import_and_submission() {
        // The verified JMAP chain (kata vt0m): one request — Email/import
        // into Drafts ($draft), then EmailSubmission/set referencing the
        // import via the "#e" creation id, moving Drafts→Sent on success.
        let (mut s, recorded) = spawn_jmap_loopback().await;
        let mut sub = invite_submission();
        sub.attachments = vec![];
        let result = send_email(&mut s, &sub, "bob@example.com", Some("ident1")).await;
        assert_eq!(
            result.expect("send should succeed").as_deref(),
            Some("E-imported-1"),
            "send_email must return the sent email id"
        );
        let reqs = recorded.lock().unwrap().clone();
        let body: serde_json::Value = serde_json::from_slice(
            &reqs
                .iter()
                .find(|r| r.path.ends_with("/jmap"))
                .expect("one JMAP call")
                .body,
        )
        .unwrap();
        let calls = &body["methodCalls"];
        assert_eq!(calls[0][0], "Email/import");
        let e = &calls[0][1]["emails"]["e"];
        assert_eq!(e["blobId"], "blob-rfc822-1");
        assert_eq!(e["mailboxIds"]["mb-drafts"], true);
        assert_eq!(e["keywords"]["$draft"], true);
        assert_eq!(calls[1][0], "EmailSubmission/set");
        let send = &calls[1][1]["create"]["send"];
        assert_eq!(
            send["emailId"], "#e",
            "submission references the import by creation id"
        );
        assert_eq!(send["identityId"], "ident1");
        assert_eq!(send["envelope"]["mailFrom"]["email"], "bob@example.com");
        let rcpt = send["envelope"]["rcptTo"]
            .as_array()
            .expect("envelope rcptTo");
        assert!(
            rcpt.iter().any(|r| r["email"] == "guest@example.com"),
            "envelope must include the to recipient: {rcpt:?}"
        );
        assert!(
            rcpt.iter().any(|r| r["email"] == "observer@example.com"),
            "envelope must include the cc recipient: {rcpt:?}"
        );
        let patch = &calls[1][1]["onSuccessUpdateEmail"]["#send"];
        assert!(
            patch["mailboxIds/mb-drafts"].is_null()
                && patch
                    .as_object()
                    .unwrap()
                    .contains_key("mailboxIds/mb-drafts"),
            "success patch must remove the message from Drafts"
        );
        assert_eq!(patch["mailboxIds/mb-sent"], true);
        assert!(
            patch["keywords/$draft"].is_null()
                && patch.as_object().unwrap().contains_key("keywords/$draft"),
            "success patch must clear $draft"
        );
    }

    #[tokio::test]
    async fn invite_sends_no_shape_fastmail_rejects() {
        // Regression signatures of BOTH production failures (kata vt0m +
        // 2xh9): no `header:*` object key (rejected
        // bodyStructure/subParts[1]/header:Content-Type) and no `;` in any
        // `type` value (rejected bodyStructure/subParts[1]/type) — across
        // every JSON body sent to the JMAP endpoint. The import path has no
        // bodyStructure at all, so this is a guard against a future regression
        // that reroutes invites back through Email/set.
        let (mut s, recorded) = spawn_jmap_loopback().await;
        let mut sub = invite_submission();
        sub.attachments = vec![];
        let result = send_email(&mut s, &sub, "bob@example.com", Some("ident1")).await;
        assert!(result.is_ok(), "send should succeed: {result:?}");
        let reqs = recorded.lock().unwrap().clone();
        let jmap_bodies: Vec<serde_json::Value> = reqs
            .iter()
            .filter(|r| r.path.ends_with("/jmap"))
            .map(|r| serde_json::from_slice(&r.body).expect("JMAP body is JSON"))
            .collect();
        assert!(!jmap_bodies.is_empty(), "expected at least one JMAP call");
        for body in &jmap_bodies {
            assert_no_rejected_part_shapes(body);
        }
    }

    #[test]
    fn build_itip_mime_inlines_attachment_bytes_and_derives_method() {
        // Pure builder unit (the network-free half of the vt0m split): given
        // the attachment bytes, the MIME inlines them byte-identical and
        // derives the method from the ICS METHOD line (REQUEST here).
        use base64::Engine;
        let bytes = b"PDF-CONTENT-123".to_vec();
        let mut sub = invite_submission();
        sub.attachments = vec![Attachment {
            blob_id: "blob-pdf-123".into(),
            name: "report.pdf".into(),
            mime_type: "application/pdf".into(),
            size: bytes.len() as i64,
        }];
        let mime = build_itip_mime(&sub, "bob@example.com", std::slice::from_ref(&bytes));
        assert!(
            mime.contains("Content-Type: text/calendar; method=REQUEST; charset=utf-8"),
            "method must be derived from the ICS METHOD line: {mime}"
        );
        assert!(
            mime.contains("Cc: observer@example.com"),
            "cc must ride on a Cc header: {mime}"
        );
        assert!(
            mime.contains("Content-Type: application/pdf; name=\"report.pdf\""),
            "attachment Content-Type with name: {mime}"
        );
        assert!(
            mime.contains("Content-Disposition: attachment; filename=\"report.pdf\""),
            "attachment Content-Disposition: {mime}"
        );
        let disp = mime.find("Content-Disposition: attachment").unwrap();
        let b64_start = mime[disp..].find("\r\n\r\n").unwrap() + disp + 4;
        let b64_end = mime[b64_start..].find("\r\n--").unwrap() + b64_start;
        let b64: String = mime[b64_start..b64_end]
            .chars()
            .filter(|c| !c.is_ascii_whitespace())
            .collect();
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(b64)
            .expect("attachment part is valid base64");
        assert_eq!(
            decoded, bytes,
            "attachment bytes must be inlined byte-identical"
        );
    }

    #[test]
    fn itip_mime_long_ascii_subject_is_folded_within_line_limits() {
        // roborev 416 #3: ASCII subjects from third-party ICS can be
        // arbitrarily long; they must not produce a header line beyond
        // RFC 5322 limits.
        let mut sub = itip_submission();
        sub.subject = format!("Re: {}", "x".repeat(400));
        let mime = build_itip_mime(&sub, "bob@example.com", &[]);
        for line in mime.lines() {
            assert!(
                line.len() <= 78,
                "header line exceeds RFC 5322 SHOULD limit ({}): {line:?}",
                line.len()
            );
        }
    }

    #[tokio::test]
    async fn itip_reply_surfaces_method_level_jmap_error() {
        // roborev 416 #4: a method-level failure is ["error", {...}] — no
        // notCreated — and must surface the error object, not "no detail".
        let (mut s, _recorded) = loopback_session(serde_json::json!({
            "blobId": "blob-rfc822-1",
            "size": 1,
            "methodResponses": [
                ["error", {"type": "serverFail", "description": "boom"}, "0"]
            ]
        }))
        .await;
        let err = send_email(
            &mut s,
            &itip_submission(),
            "bob@example.com",
            Some("ident1"),
        )
        .await
        .expect_err("method-level error must fail the send");
        let msg = format!("{err}");
        assert!(
            msg.contains("serverFail"),
            "error must carry the JMAP error detail, got: {msg}"
        );
    }

    // --- build_draft_email attachment tests ---

    fn pdf_attachment() -> Attachment {
        Attachment {
            blob_id: "blob-pdf-123".into(),
            name: "report.pdf".into(),
            mime_type: "application/pdf".into(),
            size: 12345,
        }
    }

    #[test]
    fn draft_text_with_attachment_wraps_in_mixed() {
        let sub = EmailSubmission {
            to: vec!["bob@example.com".into()],
            cc: vec![],
            subject: "With attachment".into(),
            text_body: "See attached".into(),
            bcc: None,
            html_body: None,
            in_reply_to: None,
            references: None,
            attachments: vec![pdf_attachment()],
            calendar_ics: None,
        };
        let draft = build_draft_email(&sub, "alice@example.com", "mb-drafts");
        assert_eq!(draft["bodyStructure"]["type"], "multipart/mixed");
        let parts = draft["bodyStructure"]["subParts"]
            .as_array()
            .expect("subParts");
        assert_eq!(parts.len(), 2);
        assert_eq!(parts[0]["type"], "text/plain");
        assert_eq!(parts[0]["partId"], "body");
        assert_eq!(parts[1]["type"], "application/pdf");
        assert_eq!(parts[1]["blobId"], "blob-pdf-123");
        assert_eq!(parts[1]["name"], "report.pdf");
        assert_eq!(parts[1]["disposition"], "attachment");
    }

    #[test]
    fn draft_html_with_attachment_wraps_in_mixed() {
        let sub = EmailSubmission {
            to: vec!["bob@example.com".into()],
            cc: vec![],
            subject: "HTML + attachment".into(),
            text_body: "See attached".into(),
            bcc: None,
            html_body: Some("<p>See attached</p>".into()),
            in_reply_to: None,
            references: None,
            attachments: vec![pdf_attachment()],
            calendar_ics: None,
        };
        let draft = build_draft_email(&sub, "alice@example.com", "mb-drafts");
        assert_eq!(draft["bodyStructure"]["type"], "multipart/mixed");
        let parts = draft["bodyStructure"]["subParts"]
            .as_array()
            .expect("subParts");
        assert_eq!(parts.len(), 2);
        // First part is the original multipart/alternative
        assert_eq!(parts[0]["type"], "multipart/alternative");
        assert_eq!(parts[0]["subParts"].as_array().unwrap().len(), 2);
        // Second part is the attachment
        assert_eq!(parts[1]["type"], "application/pdf");
        assert_eq!(parts[1]["blobId"], "blob-pdf-123");
    }

    #[test]
    fn draft_multiple_attachments() {
        let sub = EmailSubmission {
            to: vec!["bob@example.com".into()],
            cc: vec![],
            subject: "Multiple".into(),
            text_body: "See attached".into(),
            bcc: None,
            html_body: None,
            in_reply_to: None,
            references: None,
            attachments: vec![
                pdf_attachment(),
                Attachment {
                    blob_id: "blob-img-456".into(),
                    name: "photo.jpg".into(),
                    mime_type: "image/jpeg".into(),
                    size: 54321,
                },
                Attachment {
                    blob_id: "blob-doc-789".into(),
                    name: "notes.txt".into(),
                    mime_type: "text/plain".into(),
                    size: 100,
                },
            ],
            calendar_ics: None,
        };
        let draft = build_draft_email(&sub, "alice@example.com", "mb-drafts");
        assert_eq!(draft["bodyStructure"]["type"], "multipart/mixed");
        let parts = draft["bodyStructure"]["subParts"]
            .as_array()
            .expect("subParts");
        // body + 3 attachments
        assert_eq!(parts.len(), 4);
        assert_eq!(parts[0]["type"], "text/plain");
        assert_eq!(parts[1]["blobId"], "blob-pdf-123");
        assert_eq!(parts[2]["blobId"], "blob-img-456");
        assert_eq!(parts[3]["blobId"], "blob-doc-789");
    }

    #[test]
    fn draft_no_attachments_unchanged() {
        // Verify that empty attachments vec doesn't change existing behavior
        let sub = simple_submission();
        let draft = build_draft_email(&sub, "alice@example.com", "mb-drafts");
        assert_eq!(draft["bodyStructure"]["type"], "text/plain");
        assert!(draft["bodyStructure"].get("subParts").is_none());
    }

    // --- replace_case_insensitive tests ---

    #[test]
    fn replace_case_insensitive_basic() {
        assert_eq!(
            replace_case_insensitive(r#"src="cid:abc123""#, "cid:abc123", "/img/1.png"),
            r#"src="/img/1.png""#
        );
    }

    #[test]
    fn replace_case_insensitive_mixed_case() {
        assert_eq!(
            replace_case_insensitive(r#"src="CID:abc123""#, "cid:abc123", "/img/1.png"),
            r#"src="/img/1.png""#
        );
        assert_eq!(
            replace_case_insensitive(r#"src="Cid:abc123""#, "cid:abc123", "/img/1.png"),
            r#"src="/img/1.png""#
        );
    }

    #[test]
    fn replace_case_insensitive_multiple_occurrences() {
        let html = r#"<img src="CID:x"><img src="cid:x">"#;
        assert_eq!(
            replace_case_insensitive(html, "cid:x", "/img/x.png"),
            r#"<img src="/img/x.png"><img src="/img/x.png">"#
        );
    }

    #[test]
    fn replace_case_insensitive_no_match() {
        let html = "no cids here";
        assert_eq!(
            replace_case_insensitive(html, "cid:abc", "/img/1.png"),
            "no cids here"
        );
    }

    // --- JMAP filter translation tests (moved from search.rs) ---

    #[test]
    fn jmap_filter_empty() {
        let filter = to_jmap_filter(None, None);
        assert_eq!(filter, serde_json::json!({}));
    }

    #[test]
    fn jmap_filter_mailbox_only() {
        let filter = to_jmap_filter(None, Some("inbox-id"));
        assert_eq!(filter, serde_json::json!({"inMailbox": "inbox-id"}));
    }

    #[test]
    fn jmap_filter_from() {
        let q = ParsedQuery {
            from: vec!["john@example.com".into()],
            ..Default::default()
        };
        let filter = to_jmap_filter(Some(&q), None);
        assert_eq!(filter, serde_json::json!({"from": "john@example.com"}));
    }

    #[test]
    fn jmap_filter_unread() {
        let q = ParsedQuery {
            is_unread: Some(true),
            ..Default::default()
        };
        let filter = to_jmap_filter(Some(&q), None);
        assert_eq!(filter, serde_json::json!({"notKeyword": "$seen"}));
    }

    #[test]
    fn jmap_filter_flagged() {
        let q = ParsedQuery {
            is_flagged: Some(true),
            ..Default::default()
        };
        let filter = to_jmap_filter(Some(&q), None);
        assert_eq!(filter, serde_json::json!({"hasKeyword": "$flagged"}));
    }

    #[test]
    fn jmap_filter_attachment() {
        let q = ParsedQuery {
            has_attachment: true,
            ..Default::default()
        };
        let filter = to_jmap_filter(Some(&q), None);
        assert_eq!(filter, serde_json::json!({"hasAttachment": true}));
    }

    #[test]
    fn jmap_filter_text() {
        let q = ParsedQuery {
            text: "search terms".into(),
            ..Default::default()
        };
        let filter = to_jmap_filter(Some(&q), None);
        assert_eq!(filter, serde_json::json!({"text": "search terms"}));
    }

    #[test]
    fn jmap_filter_multiple_conditions_uses_and() {
        let q = ParsedQuery {
            from: vec!["alice@example.com".into()],
            has_attachment: true,
            ..Default::default()
        };
        let filter = to_jmap_filter(Some(&q), Some("inbox-id"));
        assert_eq!(filter["operator"], "AND");
        let conditions = filter["conditions"].as_array().unwrap();
        assert_eq!(conditions.len(), 3);
    }

    #[test]
    fn jmap_filter_date_after() {
        let q = ParsedQuery {
            after: Some(chrono::NaiveDate::from_ymd_opt(2026, 1, 15).unwrap()),
            ..Default::default()
        };
        let filter = to_jmap_filter(Some(&q), None);
        assert_eq!(filter, serde_json::json!({"after": "2026-01-15T00:00:00Z"}));
    }

    #[test]
    fn jmap_filter_date_before() {
        let q = ParsedQuery {
            before: Some(chrono::NaiveDate::from_ymd_opt(2026, 6, 30).unwrap()),
            ..Default::default()
        };
        let filter = to_jmap_filter(Some(&q), None);
        assert_eq!(
            filter,
            serde_json::json!({"before": "2026-06-30T00:00:00Z"})
        );
    }

    // --- query_emails sort clause (kata 09ef) ---

    #[test]
    fn jmap_sort_clause_date_desc_is_descending() {
        let sort = jmap_sort_clause(EmailSort::DateDesc);
        assert_eq!(
            sort,
            serde_json::json!([{ "property": "receivedAt", "isAscending": false }])
        );
    }

    #[test]
    fn jmap_sort_clause_date_asc_is_ascending() {
        let sort = jmap_sort_clause(EmailSort::DateAsc);
        assert_eq!(
            sort,
            serde_json::json!([{ "property": "receivedAt", "isAscending": true }])
        );
    }

    // --- JMAP deserialization type tests (moved from types.rs) ---

    #[test]
    fn body_structure_part_from_jmap() {
        let json = serde_json::json!({
            "type": "multipart/mixed",
            "subParts": [
                {
                    "type": "text/plain",
                    "partId": "1",
                    "blobId": "b1",
                    "size": 100
                },
                {
                    "type": "application/pdf",
                    "partId": "2",
                    "blobId": "b2",
                    "name": "report.pdf",
                    "disposition": "attachment",
                    "size": 5000
                }
            ]
        });
        let part: BodyStructurePart = serde_json::from_value(json).unwrap();
        assert_eq!(part.mime_type, "multipart/mixed");
        assert_eq!(part.sub_parts.len(), 2);
        assert_eq!(part.sub_parts[0].mime_type, "text/plain");
        assert_eq!(part.sub_parts[1].name.as_deref(), Some("report.pdf"));
        assert_eq!(part.sub_parts[1].size, 5000);
    }

    #[test]
    fn body_structure_part_defaults_on_missing_fields() {
        let json = serde_json::json!({});
        let part: BodyStructurePart = serde_json::from_value(json).unwrap();
        assert_eq!(part.mime_type, "");
        assert!(part.blob_id.is_none());
        assert!(part.name.is_none());
        assert!(part.sub_parts.is_empty());
        assert_eq!(part.size, 0);
    }

    #[test]
    fn jmap_email_raw_handles_explicit_null_fields() {
        let json = serde_json::json!({
            "id": "e1",
            "blobId": "b1",
            "threadId": "t1",
            "from": null,
            "to": null,
            "cc": null,
            "subject": null,
            "preview": null,
            "size": null
        });
        let raw: JmapEmailRaw = serde_json::from_value(json).unwrap();
        assert!(raw.from.is_empty());
        assert!(raw.to.is_empty());
        assert!(raw.cc.is_empty());
        assert_eq!(raw.subject, "");
        assert_eq!(raw.preview, "");
        assert_eq!(raw.size, 0);
    }

    #[test]
    fn jmap_email_raw_deserializes_with_only_split_count_properties() {
        // Mirrors what split_counts (src/routes.rs) sends via properties_override:
        // ["id", "from", "to", "cc", "subject"]. JMAP responds with only those
        // keys, so JmapEmailRaw must tolerate missing blobId/threadId or the
        // route 500s for every Fastmail account.
        let json = serde_json::json!({
            "id": "e1",
            "from": [{"email": "a@example.com"}],
            "to": [],
            "cc": [],
            "subject": "Hi"
        });
        let raw: JmapEmailRaw = serde_json::from_value(json)
            .expect("partial Email/get response (id/from/to/cc/subject only) must deserialize");
        assert_eq!(raw.id, "e1");
        assert_eq!(raw.blob_id, "");
        assert_eq!(raw.thread_id, "");
        assert_eq!(raw.subject, "Hi");
    }

    #[test]
    fn body_structure_part_handles_null_size_and_sub_parts() {
        let json = serde_json::json!({
            "type": "text/plain",
            "size": null,
            "subParts": null
        });
        let part: BodyStructurePart = serde_json::from_value(json).unwrap();
        assert_eq!(part.size, 0);
        assert!(part.sub_parts.is_empty());
    }

    // =========================================================================
    // kata m5yp — Fastmail CalDAV must use Basic auth with an app password
    // (the API token is JMAP/MCP-only and Fastmail rejects it at the CalDAV
    // endpoint). Behavioral RED/GREEN tests against a real loopback HTTP
    // recorder — no mocking framework. See `caldav_recorder` above.
    // =========================================================================

    const TEST_ICS: &str = "BEGIN:VCALENDAR\r\nBEGIN:VEVENT\r\nUID:uid-m5yp\r\nSUMMARY:Test\r\n\
         DTSTART:20260101T100000Z\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";

    /// The collection-id segment the discovery recorders serve as the default
    /// writable calendar — so tests can assert the resolved PUT/GET/DELETE
    /// address it (never `/Default/`).
    const DEFAULT_COLL_SEGMENT: &str = "collid-default";

    /// Find the recorded requests with a given method (e.g. `"PUT"`),
    /// ignoring the discovery PROPFINDs that precede them — so a test can
    /// assert on the actual calendar op after discovery runs.
    fn recorded_by_method<'a>(
        rec: &'a [caldav_recorder::RecordedRequest],
        method: &str,
    ) -> Vec<&'a caldav_recorder::RecordedRequest> {
        rec.iter().filter(|r| r.method == method).collect()
    }

    /// Count the recorded PROPFINDs — used to assert discovery ran (and ran
    /// once, when cached across ops on the same session).
    fn propfind_count(rec: &[caldav_recorder::RecordedRequest]) -> usize {
        rec.iter().filter(|r| r.method == "PROPFIND").count()
    }

    /// Spawn a scripted recorder serving the empirically-verified CalDAV
    /// discovery chain (kata wybm): PROPFIND `/dav/calendars` →
    /// `current-user-principal` at `principal_href`; PROPFIND the derived home
    /// → a realistic Cyrus home listing (the home itself, the default writable
    /// calendar "General (FastMail)" at `default_href`, a tasks calendar
    /// "DEFAULT_TASK_CALENDAR_NAME", and schedule-inbox/outbox — so the parser
    /// must exclude the non-event collections to pick the default); and any
    /// PUT/GET/DELETE → `write_status` / `write_body`. `principal_href` is the
    /// server-absolute path (the Cyrus underscore-munged user segment), NOT the
    /// session username — that's the whole point of the fix.
    async fn spawn_discovery(
        principal_href: &str,
        default_href: &str,
        write_status: axum::http::StatusCode,
        write_body: Vec<u8>,
    ) -> (
        String,
        std::sync::Arc<std::sync::Mutex<Vec<caldav_recorder::RecordedRequest>>>,
    ) {
        use caldav_recorder::{CalCollection, CalKind};
        let home = principal_href.replace("/principals/", "/calendars/");
        let collections = vec![
            CalCollection {
                href: home.clone(),
                displayname: "Home".into(),
                kind: CalKind::PlainCollection,
                writable: true,
            },
            CalCollection {
                href: default_href.into(),
                displayname: "General (FastMail)".into(),
                kind: CalKind::Calendar,
                writable: true,
            },
            CalCollection {
                href: format!("{home}TASKS/"),
                displayname: "DEFAULT_TASK_CALENDAR_NAME".into(),
                kind: CalKind::Calendar,
                writable: true,
            },
            CalCollection {
                href: format!("{home}Inbox/"),
                displayname: "Inbox".into(),
                kind: CalKind::ScheduleInbox,
                writable: true,
            },
            CalCollection {
                href: format!("{home}Outbox/"),
                displayname: "Outbox".into(),
                kind: CalKind::ScheduleOutbox,
                writable: true,
            },
        ];
        let principal_xml =
            std::sync::Arc::new(caldav_recorder::principal_multistatus(principal_href));
        let home_xml = std::sync::Arc::new(caldav_recorder::home_multistatus(&collections));
        let write_body = std::sync::Arc::new(write_body);
        let home_path = home.clone();
        caldav_recorder::spawn_scripted(move |method, path| {
            if method == "PROPFIND" && path == "/dav/calendars" {
                (
                    axum::http::StatusCode::MULTI_STATUS,
                    principal_xml.as_bytes().to_vec(),
                )
            } else if method == "PROPFIND" && path == home_path {
                (
                    axum::http::StatusCode::MULTI_STATUS,
                    home_xml.as_bytes().to_vec(),
                )
            } else {
                (write_status, write_body.as_ref().clone())
            }
        })
        .await
    }

    /// The expected discovered collection path for a munged-user scenario:
    /// `/dav/calendars/user/{munged_user}/collid-default/`.
    fn default_coll_path(munged_user: &str) -> String {
        format!("/dav/calendars/user/{munged_user}/{DEFAULT_COLL_SEGMENT}/")
    }

    #[tokio::test]
    async fn caldav_put_uses_basic_auth_with_app_password() {
        // With an app password, add_to_calendar must (a) discover the default
        // calendar collection via PROPFIND (kata wybm), then (b) PUT the
        // METHOD-stripped ICS to the *resolved* URL using Basic auth with the
        // app password — never the Bearer api-token, never `/Default/`.
        let (base, recorded) = spawn_discovery(
            "/dav/principals/user/user@fastmail.com/",
            &default_coll_path("user@fastmail.com"),
            axum::http::StatusCode::CREATED,
            Vec::new(),
        )
        .await;
        let mut sess = JmapSession::new(
            "user@fastmail.com",
            "fmu1-test-token",
            Some("test-app-pass"),
        );
        sess.caldav_base = base;

        let result = add_to_calendar(&sess, TEST_ICS, "uid-m5yp", false).await;
        assert!(
            result.is_ok(),
            "add_to_calendar should succeed against the loopback: {result:?}"
        );

        let rec = recorded.lock().unwrap();
        // Discovery ran: PROPFIND the calendars root, then the derived home.
        assert_eq!(
            propfind_count(&rec),
            2,
            "discovery must issue two PROPFINDs (root + home), got {rec:?}"
        );
        let puts = recorded_by_method(&rec, "PUT");
        assert_eq!(
            puts.len(),
            1,
            "exactly one CalDAV PUT expected, got {rec:?}"
        );
        let put = puts[0];
        // The PUT addresses the DISCOVERED collection — not the /Default/
        // literal (the wybm bug) and not a session-username-concatenated path.
        assert!(
            put.path
                .ends_with("/dav/calendars/user/user@fastmail.com/collid-default/uid-m5yp.ics"),
            "CalDAV PUT must address the discovered collection, not /Default/: {}",
            put.path
        );
        assert!(
            !put.path.contains("/Default/"),
            "the /Default/ literal must be gone: {}",
            put.path
        );
        let auth = put
            .authorization
            .as_deref()
            .expect("CalDAV PUT must send an Authorization header");
        assert_eq!(
            auth,
            caldav_recorder::expected_basic_header("user@fastmail.com", "test-app-pass"),
            "CalDAV PUT must use Basic auth with the app password, not the Bearer api-token"
        );
        assert!(
            !auth.contains("Bearer"),
            "CalDAV PUT must not send the Bearer api-token"
        );
        // METHOD:PUMPED — strip_method must remove the iTIP transport property
        // before storage (RFC 4791). Body is the stored ICS, so no METHOD line.
        let body = std::str::from_utf8(&put.body).unwrap_or("");
        assert!(
            !body.contains("METHOD:"),
            "stored ICS must have METHOD stripped"
        );
        assert!(
            body.contains("BEGIN:VEVENT"),
            "stored ICS body must be forwarded"
        );
    }

    #[tokio::test]
    async fn caldav_write_without_app_password_is_surfaced_not_swallowed() {
        // RED today: add_to_calendar issues the PUT anyway (Bearer), gets a
        // 401 from the real host, and returns Ok(false) — the caller warns
        // and the UI reports success. Must instead return
        // Err(CalendarAuthUnconfigured) without issuing any HTTP request.
        let (base, recorded) = caldav_recorder::spawn(axum::http::StatusCode::OK, Vec::new()).await;
        let mut sess = JmapSession::new("user@fastmail.com", "fmu1-test-token", None);
        sess.caldav_base = base;

        let result = add_to_calendar(&sess, TEST_ICS, "uid-m5yp", false).await;
        let err = result.expect_err(
            "missing app password must surface as Err(CalendarAuthUnconfigured), not Ok(false)",
        );
        assert!(
            matches!(err, Error::CalendarAuthUnconfigured),
            "expected Error::CalendarAuthUnconfigured, got {err:?}"
        );
        assert!(
            recorded.lock().unwrap().is_empty(),
            "no HTTP request may be issued when the app password is unconfigured"
        );
    }

    #[tokio::test]
    async fn caldav_get_and_delete_use_same_basic_auth() {
        // Sibling calls can't regress to the Bearer api-token, and must both
        // address the discovered (cached) collection — never /Default/.
        let (base, recorded) = spawn_discovery(
            "/dav/principals/user/user@fastmail.com/",
            &default_coll_path("user@fastmail.com"),
            axum::http::StatusCode::OK,
            TEST_ICS.as_bytes().to_vec(),
        )
        .await;
        let mut sess = JmapSession::new(
            "user@fastmail.com",
            "fmu1-test-token",
            Some("test-app-pass"),
        );
        sess.caldav_base = base;
        let expected = caldav_recorder::expected_basic_header("user@fastmail.com", "test-app-pass");

        let _ = get_calendar_event(&sess, "uid-m5yp", chrono_tz::Tz::UTC).await;
        let _ = remove_from_calendar(&sess, "uid-m5yp").await;

        let rec = recorded.lock().unwrap();
        // Discovery ran once (cached across the two ops on the same session):
        // two PROPFINDs total, not four.
        assert_eq!(
            propfind_count(&rec),
            2,
            "discovery must be cached across ops (two PROPFINDs total), got {rec:?}"
        );
        let gets = recorded_by_method(&rec, "GET");
        let deletes = recorded_by_method(&rec, "DELETE");
        assert_eq!(gets.len(), 1, "one GET expected, got {rec:?}");
        assert_eq!(deletes.len(), 1, "one DELETE expected, got {rec:?}");
        for r in [gets[0], deletes[0]] {
            assert!(
                r.path
                    .ends_with("/dav/calendars/user/user@fastmail.com/collid-default/uid-m5yp.ics"),
                "{} must address the discovered collection, not /Default/: {}",
                r.method,
                r.path
            );
            assert!(
                !r.path.contains("/Default/"),
                "the /Default/ literal must be gone: {}",
                r.path
            );
            assert_eq!(
                r.authorization.as_deref(),
                Some(expected.as_str()),
                "{} must use Basic auth with the app password",
                r.method
            );
            assert!(
                !r.authorization.as_deref().unwrap_or("").contains("Bearer"),
                "{} must not send the Bearer api-token",
                r.method
            );
        }
    }

    #[tokio::test]
    async fn caldav_get_rsvp_status_uses_basic_auth_and_refuses_without_app_password() {
        // `get_rsvp_status` is the fourth CalDAV call (kata m5yp "done when":
        // all four must use Basic + surface CalendarAuthUnconfigured, and now
        // also address the discovered collection — kata wybm). With an app
        // password it discovers then GETs the resolved URL with Basic; without,
        // it returns Err(CalendarAuthUnconfigured) and issues no request.
        let (base, recorded) = spawn_discovery(
            "/dav/principals/user/user@fastmail.com/",
            &default_coll_path("user@fastmail.com"),
            axum::http::StatusCode::OK,
            TEST_ICS.as_bytes().to_vec(),
        )
        .await;
        let mut sess = JmapSession::new(
            "user@fastmail.com",
            "fmu1-test-token",
            Some("test-app-pass"),
        );
        sess.caldav_base = base.clone();
        let expected = caldav_recorder::expected_basic_header("user@fastmail.com", "test-app-pass");

        let _ = get_rsvp_status(&sess, "uid-m5yp", "user@fastmail.com", chrono_tz::Tz::UTC).await;
        {
            let rec = recorded.lock().unwrap();
            let gets = recorded_by_method(&rec, "GET");
            assert_eq!(gets.len(), 1, "one CalDAV GET expected, got {rec:?}");
            assert!(
                gets[0]
                    .path
                    .ends_with("/dav/calendars/user/user@fastmail.com/collid-default/uid-m5yp.ics"),
                "get_rsvp_status must address the discovered collection, not /Default/: {}",
                gets[0].path
            );
            assert_eq!(
                gets[0].authorization.as_deref(),
                Some(expected.as_str()),
                "get_rsvp_status must use Basic auth with the app password"
            );
        }

        // Now without an app password: named error, no HTTP.
        let (base2, recorded2) =
            caldav_recorder::spawn(axum::http::StatusCode::OK, Vec::new()).await;
        let mut sess2 = JmapSession::new("user@fastmail.com", "fmu1-test-token", None);
        sess2.caldav_base = base2;
        let result =
            get_rsvp_status(&sess2, "uid-m5yp", "user@fastmail.com", chrono_tz::Tz::UTC).await;
        let err = result.expect_err("missing app password must surface, not Ok(None)");
        assert!(
            matches!(err, Error::CalendarAuthUnconfigured),
            "expected CalendarAuthUnconfigured, got {err:?}"
        );
        assert!(
            recorded2.lock().unwrap().is_empty(),
            "no HTTP request may be issued when the app password is unconfigured"
        );
    }

    #[tokio::test]
    async fn caldav_url_encodes_reserved_uid_chars_consistently_across_put_get_delete() {
        // roborev 376: get_calendar_event used to interpolate the raw uid while
        // its siblings percent-encoded it, so a UID with reserved chars (space,
        // '/', '@' — legal in iCalendar) was stored by PUT under the encoded
        // name but looked up by GET under the raw one → perpetual 404 → the
        // first-time auto-add re-ran on every email open. All four must encode
        // identically so PUT and GET address the same resource — now under the
        // discovered collection (kata wybm), not /Default/.
        let (base, recorded) = spawn_discovery(
            "/dav/principals/user/user@fastmail.com/",
            &default_coll_path("user@fastmail.com"),
            axum::http::StatusCode::OK,
            TEST_ICS.as_bytes().to_vec(),
        )
        .await;
        let mut sess = JmapSession::new(
            "user@fastmail.com",
            "fmu1-test-token",
            Some("test-app-pass"),
        );
        sess.caldav_base = base;
        // UID with a space and an '@' — both must be percent-encoded in the path.
        let uid = "uid with space@google.com";
        let encoded = percent_encode_path(uid);
        assert!(encoded.contains("%20") && encoded.contains("%40"));

        let _ = add_to_calendar(&sess, TEST_ICS, uid, false).await;
        let _ = get_calendar_event(&sess, uid, chrono_tz::Tz::UTC).await;
        let _ = remove_from_calendar(&sess, uid).await;

        let rec = recorded.lock().unwrap();
        let ops: Vec<&caldav_recorder::RecordedRequest> = rec
            .iter()
            .filter(|r| matches!(r.method.as_str(), "PUT" | "GET" | "DELETE"))
            .collect();
        assert_eq!(ops.len(), 3, "PUT + GET + DELETE expected, got {rec:?}");
        for r in ops.iter() {
            assert!(
                r.path.ends_with(&format!("/collid-default/{encoded}.ics")),
                "{} {:?} must address the encoded UID under the discovered collection, not the raw one: {}",
                r.method,
                r.path,
                uid
            );
            assert!(
                !r.path.contains(' '),
                "raw space must not appear in the path"
            );
            assert!(
                !r.path.contains("/Default/"),
                "the /Default/ literal must be gone: {}",
                r.path
            );
        }
    }

    // =========================================================================
    // kata wybm — CalDAV calendar-home discovery (replace hardcoded /Default/)
    //
    // The four tests below pin the empirically-verified Fastmail/Cyrus chain
    // (PROPFIND calendars root → current-user-principal → derive home →
    // PROPFIND home → pick default writable calendar) against loopback
    // recorders, plus the caching and honest-failure contracts. The parser
    // unit tests above them pin the XML parsing and the default-selection
    // heuristic in isolation, including prefix-independence and the default-
    // namespace serialization of the RFC 4791 `calendar` element (the vt0m
    // lesson: pin the real server shape, not an RFC-reasoned guess).
    // =========================================================================

    // --- parser unit tests ---

    #[test]
    fn element_text_strips_cdata_and_trims() {
        assert_eq!(
            element_text("<![CDATA[General (FastMail)]]>"),
            "General (FastMail)"
        );
        assert_eq!(
            element_text("  /dav/principals/user/u/  "),
            "/dav/principals/user/u/"
        );
        assert_eq!(element_text("plain"), "plain");
    }

    #[test]
    fn href_last_segment_trims_trailing_slash() {
        assert_eq!(href_last_segment("/dav/calendars/user/u/COLLID/"), "COLLID");
        assert_eq!(
            href_last_segment("/dav/calendars/user/u.name@x.Default/"),
            "u.name@x.Default"
        );
        assert_eq!(href_last_segment("/dav/calendars/user/u/"), "u");
    }

    #[test]
    fn parse_current_user_principal_extracts_href() {
        // The href Cyrus returns carries the underscore-munged user segment —
        // the parser must surface it verbatim (the client never munges).
        let xml =
            caldav_recorder::principal_multistatus("/dav/principals/user/u_name@fastmail.com/");
        assert_eq!(
            parse_current_user_principal(&xml).as_deref(),
            Some("/dav/principals/user/u_name@fastmail.com/")
        );
    }

    #[test]
    fn parse_current_user_principal_none_when_absent() {
        let xml = "<?xml version=\"1.0\"?><D:multistatus xmlns:D=\"DAV:\"><D:response>\
<D:href>/dav/calendars</D:href></D:response></D:multistatus>";
        assert!(parse_current_user_principal(xml).is_none());
    }

    #[test]
    fn derive_calendar_home_substitutes_principals_for_calendars() {
        assert_eq!(
            derive_calendar_home("/dav/principals/user/u_name@fastmail.com/").as_deref(),
            Some("/dav/calendars/user/u_name@fastmail.com/")
        );
    }

    #[test]
    fn derive_calendar_home_none_when_not_a_principal_href() {
        assert!(derive_calendar_home("/dav/calendars/user/u/").is_none());
        assert!(derive_calendar_home("").is_none());
    }

    #[test]
    fn parse_calendar_collections_handles_rfc4791_namespace_and_flags() {
        // The builder emits `calendar` under RFC 4791's
        // `urn:ietf:params:xml:ns:caldav` namespace (C:), CDATA displaynames,
        // and `write`/`write-properties`/`all` privileges — the real shape.
        use caldav_recorder::{CalCollection, CalKind};
        let collections = vec![
            CalCollection {
                href: "/dav/calendars/user/u/".into(),
                displayname: "Home".into(),
                kind: CalKind::PlainCollection,
                writable: true,
            },
            CalCollection {
                href: "/dav/calendars/user/u/COLLID/".into(),
                displayname: "General (FastMail)".into(),
                kind: CalKind::Calendar,
                writable: true,
            },
            CalCollection {
                href: "/dav/calendars/user/u/TASKS/".into(),
                displayname: "DEFAULT_TASK_CALENDAR_NAME".into(),
                kind: CalKind::Calendar,
                writable: true,
            },
            CalCollection {
                href: "/dav/calendars/user/u/Inbox/".into(),
                displayname: "Inbox".into(),
                kind: CalKind::ScheduleInbox,
                writable: true,
            },
            CalCollection {
                href: "/dav/calendars/user/u/RO/".into(),
                displayname: "Read Only".into(),
                kind: CalKind::Calendar,
                writable: false,
            },
        ];
        let xml = caldav_recorder::home_multistatus(&collections);
        let parsed = parse_calendar_collections(&xml);
        let by_href = |h: &str| parsed.iter().find(|c| c.href == h);
        let home = by_href("/dav/calendars/user/u/").expect("home parsed");
        assert!(!home.is_calendar, "home is a plain collection");
        assert!(home.writable);
        let general = by_href("/dav/calendars/user/u/COLLID/").expect("general parsed");
        assert!(
            general.is_calendar,
            "General is a calendar (RFC 4791 namespace matched by local name)"
        );
        assert!(general.writable);
        assert_eq!(general.displayname.as_deref(), Some("General (FastMail)"));
        let inbox = by_href("/dav/calendars/user/u/Inbox/").expect("inbox parsed");
        assert!(inbox.is_schedule, "inbox flagged as schedule");
        assert!(!inbox.is_calendar, "inbox is not a calendar");
        let ro = by_href("/dav/calendars/user/u/RO/").expect("read-only parsed");
        assert!(ro.is_calendar, "read-only is still a calendar");
        assert!(!ro.writable, "read-only is not writable");
    }

    #[test]
    fn parse_calendar_collections_matches_calendar_across_prefixes_and_default_ns() {
        // Robustness: the parser matches `calendar` by *local* element name,
        // so it's immune to the server's prefix choice and to a default-
        // namespace serialization — both legit RFC 4791 serializations of the
        // same `urn:ietf:params:xml:ns:caldav` `calendar` element. Cyrus binds
        // it to `C:`; another server might bind it to `cal:` or use the
        // default namespace (`<calendar xmlns="…"/>`). All must discover.
        //
        // Two collections in one response: one with an alternate prefix
        // `cal:`, one with the default-namespace serialization
        // `<calendar xmlns="…"/>` (the attribute-on-element form the strict
        // regex would miss — roborev finding).
        let xml = "<?xml version=\"1.0\" encoding=\"utf-8\"?>\
<D:multistatus xmlns:D=\"DAV:\" xmlns:cal=\"urn:ietf:params:xml:ns:caldav\">\
<D:response><D:href>/dav/calendars/user/u/C1/</D:href><D:propstat><D:prop>\
<D:resourcetype><D:collection/><cal:calendar/></D:resourcetype>\
<D:current-user-privilege-set><D:privilege><D:write/></D:privilege></D:current-user-privilege-set>\
</D:prop><D:status>HTTP/1.1 200 OK</D:status></D:propstat></D:response>\
<D:response><D:href>/dav/calendars/user/u/C2/</D:href><D:propstat><D:prop>\
<D:resourcetype><D:collection/><calendar xmlns=\"urn:ietf:params:xml:ns:caldav\"/></D:resourcetype>\
<D:current-user-privilege-set><D:privilege><D:write/></D:privilege></D:current-user-privilege-set>\
</D:prop><D:status>HTTP/1.1 200 OK</D:status></D:propstat></D:response>\
</D:multistatus>";
        let parsed = parse_calendar_collections(xml);
        assert_eq!(parsed.len(), 2, "both responses must parse");
        assert!(
            parsed.iter().all(|c| c.is_calendar),
            "alternate-prefix `cal:calendar` AND default-namespace `<calendar/>` \
             must both be recognized as calendars: {parsed:?}"
        );
        assert!(parsed.iter().all(|c| c.writable));
    }

    // --- default-selection heuristic unit tests ---

    fn cal(href: &str, displayname: &str, is_calendar: bool, writable: bool) -> CalCollectionInfo {
        CalCollectionInfo {
            href: href.into(),
            displayname: Some(displayname.into()),
            is_calendar,
            is_schedule: false,
            writable,
        }
    }

    #[test]
    fn pick_default_prefers_general_fastmail_marker_over_tasks() {
        let colls = [
            cal("/h/COLLID/", "General (FastMail)", true, true),
            cal("/h/TASKS/", "DEFAULT_TASK_CALENDAR_NAME", true, true),
            cal("/h/C2/", "Events", true, true),
        ];
        let picked = pick_default_calendar(&colls).expect("a default exists");
        assert_eq!(
            picked.href, "/h/COLLID/",
            "(FastMail) marker wins, not the tasks calendar"
        );
    }

    #[test]
    fn pick_default_prefers_displayname_default_literal() {
        let colls = [
            cal("/h/Default/", "Default", true, true),
            cal("/h/COLLID/", "General (FastMail)", true, true),
        ];
        let picked = pick_default_calendar(&colls).expect("a default exists");
        assert_eq!(
            picked.href, "/h/Default/",
            "displayname Default wins over the FastMail marker"
        );
    }

    #[test]
    fn pick_default_prefers_href_username_dot_default() {
        // The Fastmail `username.Default` convention (the 301 target shape) —
        // recognized by href segment, even with a non-`Default` displayname.
        let colls = [
            cal("/h/u.name@x.Default/", "My Cal", true, true),
            cal("/h/C2/", "Events", true, true),
        ];
        let picked = pick_default_calendar(&colls).expect("a default exists");
        assert_eq!(
            picked.href, "/h/u.name@x.Default/",
            ".Default href segment wins"
        );
    }

    #[test]
    fn pick_default_single_writable_when_unmarked() {
        let colls = [cal("/h/only/", "Only", true, true)];
        let picked = pick_default_calendar(&colls).expect("single writable is the default");
        assert_eq!(picked.href, "/h/only/");
    }

    #[test]
    fn pick_default_none_when_multiple_unmarked() {
        // Honest failure: two writable calendars, no Default/FastMail marker —
        // don't guess.
        let colls = [
            cal("/h/a/", "Work", true, true),
            cal("/h/b/", "Personal", true, true),
        ];
        assert!(
            pick_default_calendar(&colls).is_none(),
            "multiple unmarked writable calendars must surface, not guess"
        );
    }

    #[test]
    fn pick_default_none_when_only_read_only() {
        let colls = [cal("/h/ro/", "Read Only", true, false)];
        assert!(
            pick_default_calendar(&colls).is_none(),
            "a read-only calendar must not be picked for writes"
        );
    }

    #[test]
    fn pick_default_none_when_only_schedule_collections() {
        let colls = [
            CalCollectionInfo {
                href: "/h/Inbox/".into(),
                displayname: Some("Inbox".into()),
                is_calendar: false,
                is_schedule: true,
                writable: true,
            },
            CalCollectionInfo {
                href: "/h/Outbox/".into(),
                displayname: Some("Outbox".into()),
                is_calendar: false,
                is_schedule: true,
                writable: true,
            },
        ];
        assert!(
            pick_default_calendar(&colls).is_none(),
            "schedule-inbox/outbox must not be picked as the default calendar"
        );
    }

    // --- behavioral discovery tests (loopback recorders) ---

    #[tokio::test]
    async fn caldav_discovery_resolves_default_collection_not_default_literal() {
        // The core wybm fix: the session username has a DOT (`u.name@…`) but
        // the server's principal href uses the Cyrus UNDERSCORE-munged form
        // (`u_name@…`). The client must read `current-user-principal` and
        // address the resolved collection — never concatenate the session
        // username, never use `/Default/`. Fails on the old hardcoded code.
        let (base, recorded) = spawn_discovery(
            "/dav/principals/user/u_name@fastmail.com/",
            &default_coll_path("u_name@fastmail.com"),
            axum::http::StatusCode::CREATED,
            Vec::new(),
        )
        .await;
        let mut sess = JmapSession::new(
            "u.name@fastmail.com",
            "fmu1-test-token",
            Some("test-app-pass"),
        );
        sess.caldav_base = base;

        let result = add_to_calendar(&sess, TEST_ICS, "uid-wybm", false).await;
        assert!(result.is_ok(), "add_to_calendar should succeed: {result:?}");

        let rec = recorded.lock().unwrap();
        let puts = recorded_by_method(&rec, "PUT");
        assert_eq!(puts.len(), 1, "one PUT expected, got {rec:?}");
        let put = puts[0];
        // The PUT addresses the munged-user discovered collection — not the
        // session username (`u.name@`) and not `/Default/`.
        assert!(
            put.path.contains("u_name@fastmail.com"),
            "PUT must use the server-given (underscore-munged) user segment: {}",
            put.path
        );
        assert!(
            !put.path.contains("u.name@"),
            "PUT must NOT use the dotted session username: {}",
            put.path
        );
        assert!(
            put.path
                .ends_with("/dav/calendars/user/u_name@fastmail.com/collid-default/uid-wybm.ics"),
            "PUT must address the discovered default collection: {}",
            put.path
        );
        assert!(
            !put.path.contains("/Default/"),
            "the /Default/ literal must be gone: {}",
            put.path
        );
    }

    #[tokio::test]
    async fn caldav_discovery_caches_resolved_url_across_calls() {
        // One discovery per session, not per call: across add + get + remove on
        // the same session, the two PROPFINDs (root + home) are issued exactly
        // once, and all three ops address the same resolved collection URL.
        let (base, recorded) = spawn_discovery(
            "/dav/principals/user/u_name@fastmail.com/",
            &default_coll_path("u_name@fastmail.com"),
            axum::http::StatusCode::OK,
            TEST_ICS.as_bytes().to_vec(),
        )
        .await;
        let mut sess = JmapSession::new(
            "u.name@fastmail.com",
            "fmu1-test-token",
            Some("test-app-pass"),
        );
        sess.caldav_base = base;

        let _ = add_to_calendar(&sess, TEST_ICS, "uid-wybm", false).await;
        let _ = get_calendar_event(&sess, "uid-wybm", chrono_tz::Tz::UTC).await;
        let _ = remove_from_calendar(&sess, "uid-wybm").await;

        let rec = recorded.lock().unwrap();
        assert_eq!(
            propfind_count(&rec),
            2,
            "discovery PROPFINDs must be issued once (cached), not per call: {rec:?}"
        );
        let ops: Vec<&caldav_recorder::RecordedRequest> = rec
            .iter()
            .filter(|r| matches!(r.method.as_str(), "PUT" | "GET" | "DELETE"))
            .collect();
        assert_eq!(ops.len(), 3, "PUT + GET + DELETE expected, got {rec:?}");
        for r in ops.iter() {
            assert!(
                r.path.ends_with(
                    "/dav/calendars/user/u_name@fastmail.com/collid-default/uid-wybm.ics"
                ),
                "{} must address the same cached resolved URL: {}",
                r.method,
                r.path
            );
        }
    }

    #[tokio::test]
    async fn caldav_discovery_failure_surfaces_not_swallowed() {
        // The home PROPFIND returns 404 — discovery must surface
        // CalendarDiscoveryFailed and issue NO PUT. Fails on the old code,
        // which PUT to /Default/, followed the 301, 404'd, and returned
        // Ok(false) + warn (swallowed).
        let principal_xml = std::sync::Arc::new(caldav_recorder::principal_multistatus(
            "/dav/principals/user/u_name@fastmail.com/",
        ));
        let (base, recorded) = caldav_recorder::spawn_scripted(move |method, path| {
            if method == "PROPFIND" && path == "/dav/calendars" {
                (
                    axum::http::StatusCode::MULTI_STATUS,
                    principal_xml.as_bytes().to_vec(),
                )
            } else {
                // The derived home PROPFIND 404s ("Mailbox does not exist" —
                // the real Cyrus 404 shape).
                (
                    axum::http::StatusCode::NOT_FOUND,
                    b"Mailbox does not exist".to_vec(),
                )
            }
        })
        .await;
        let mut sess = JmapSession::new(
            "u.name@fastmail.com",
            "fmu1-test-token",
            Some("test-app-pass"),
        );
        sess.caldav_base = base;

        let result = add_to_calendar(&sess, TEST_ICS, "uid-wybm", false).await;
        let err = result.expect_err("home-PROPFIND 404 must surface, not Ok(true)");
        assert!(
            matches!(err, Error::CalendarDiscoveryFailed(_)),
            "expected CalendarDiscoveryFailed, got {err:?}"
        );
        let rec = recorded.lock().unwrap();
        assert_eq!(
            propfind_count(&rec),
            2,
            "both discovery PROPFINDs were attempted: {rec:?}"
        );
        assert!(
            recorded_by_method(&rec, "PUT").is_empty(),
            "no PUT may be issued when discovery fails: {rec:?}"
        );
    }

    #[tokio::test]
    async fn caldav_no_writable_collection_surfaces() {
        // The home lists only a read-only calendar and schedule collections —
        // no writable default calendar collection. Discovery must surface
        // CalendarDiscoveryFailed and issue no PUT (not guess /Default/).
        use caldav_recorder::{CalCollection, CalKind};
        let principal_xml = std::sync::Arc::new(caldav_recorder::principal_multistatus(
            "/dav/principals/user/u_name@fastmail.com/",
        ));
        let collections = vec![
            CalCollection {
                href: "/dav/calendars/user/u_name@fastmail.com/".into(),
                displayname: "Home".into(),
                kind: CalKind::PlainCollection,
                writable: true,
            },
            CalCollection {
                href: "/dav/calendars/user/u_name@fastmail.com/RO/".into(),
                displayname: "Read Only".into(),
                kind: CalKind::Calendar,
                writable: false,
            },
            CalCollection {
                href: "/dav/calendars/user/u_name@fastmail.com/Inbox/".into(),
                displayname: "Inbox".into(),
                kind: CalKind::ScheduleInbox,
                writable: true,
            },
        ];
        let home_xml = std::sync::Arc::new(caldav_recorder::home_multistatus(&collections));
        let home_path = "/dav/calendars/user/u_name@fastmail.com/".to_string();
        let (base, recorded) = caldav_recorder::spawn_scripted(move |method, path| {
            if method == "PROPFIND" && path == "/dav/calendars" {
                (
                    axum::http::StatusCode::MULTI_STATUS,
                    principal_xml.as_bytes().to_vec(),
                )
            } else if method == "PROPFIND" && path == home_path {
                (
                    axum::http::StatusCode::MULTI_STATUS,
                    home_xml.as_bytes().to_vec(),
                )
            } else {
                (axum::http::StatusCode::CREATED, Vec::new())
            }
        })
        .await;
        let mut sess = JmapSession::new(
            "u.name@fastmail.com",
            "fmu1-test-token",
            Some("test-app-pass"),
        );
        sess.caldav_base = base;

        let result = add_to_calendar(&sess, TEST_ICS, "uid-wybm", false).await;
        let err = result.expect_err("no writable calendar must surface, not Ok(true)");
        assert!(
            matches!(err, Error::CalendarDiscoveryFailed(_)),
            "expected CalendarDiscoveryFailed, got {err:?}"
        );
        let rec = recorded.lock().unwrap();
        assert_eq!(
            propfind_count(&rec),
            2,
            "discovery ran (both PROPFINDs) before giving up: {rec:?}"
        );
        assert!(
            recorded_by_method(&rec, "PUT").is_empty(),
            "no PUT may be issued when no writable calendar exists: {rec:?}"
        );
    }

    #[tokio::test]
    async fn caldav_discovery_coordinates_concurrent_first_callers() {
        // The OnceCell swap (roborev 431): two CalDAV calls racing on a fresh
        // session both need the collection URL, but `get_or_try_init`
        // coordinates them - only one runs the two-PROPFIND discovery while
        // the other awaits its result. Asserts propfind_count == 2 (not 4).
        // The old `Mutex<Option<String>>` would race and issue four PROPFINDs.
        let (base, recorded) = spawn_discovery(
            "/dav/principals/user/u_name@fastmail.com/",
            &default_coll_path("u_name@fastmail.com"),
            axum::http::StatusCode::OK,
            TEST_ICS.as_bytes().to_vec(),
        )
        .await;
        let mut sess = JmapSession::new(
            "u.name@fastmail.com",
            "fmu1-test-token",
            Some("test-app-pass"),
        );
        sess.caldav_base = base;

        // Two concurrent ops on the same fresh session — both reach
        // resolve_calendar_collection before the cache is populated.
        let (put, get) = tokio::join!(
            add_to_calendar(&sess, TEST_ICS, "uid-wybm", false),
            get_calendar_event(&sess, "uid-wybm", chrono_tz::Tz::UTC),
        );
        assert!(put.is_ok(), "concurrent add should succeed: {put:?}");
        assert!(get.is_ok(), "concurrent get should succeed: {get:?}");

        let rec = recorded.lock().unwrap();
        assert_eq!(
            propfind_count(&rec),
            2,
            "concurrent first-callers must share ONE discovery (two PROPFINDs), not race to four: {rec:?}"
        );
        // Both ops still landed on the resolved collection.
        assert_eq!(recorded_by_method(&rec, "PUT").len(), 1);
        assert_eq!(recorded_by_method(&rec, "GET").len(), 1);
    }

    #[tokio::test]
    async fn caldav_discovery_failure_is_not_cached_retries_next_call() {
        // The retry-on-failure contract (roborev 431): a failed discovery must
        // NOT cache the failure — `get_or_try_init` leaves the OnceCell empty
        // on Err (and releases the init permit on cancellation, no poisoning),
        // so the next call re-runs discovery. A recorder that 404s the home
        // PROPFIND once, then serves the real multistatus: first
        // add_to_calendar yields CalendarDiscoveryFailed (no PUT), second
        // succeeds (PUT lands), and four PROPFINDs total were issued (two per
        // attempt). Pins the contract so a future swap to `get_or_init` (which
        // can't return Err / would cache a failure) regresses loudly.
        use caldav_recorder::{CalCollection, CalKind};
        use std::sync::atomic::{AtomicUsize, Ordering};

        let principal_xml = std::sync::Arc::new(caldav_recorder::principal_multistatus(
            "/dav/principals/user/u_name@fastmail.com/",
        ));
        let collections = vec![
            CalCollection {
                href: "/dav/calendars/user/u_name@fastmail.com/".into(),
                displayname: "Home".into(),
                kind: CalKind::PlainCollection,
                writable: true,
            },
            CalCollection {
                href: default_coll_path("u_name@fastmail.com"),
                displayname: "General (FastMail)".into(),
                kind: CalKind::Calendar,
                writable: true,
            },
        ];
        let home_xml = std::sync::Arc::new(caldav_recorder::home_multistatus(&collections));
        let home_path = "/dav/calendars/user/u_name@fastmail.com/".to_string();
        // First home PROPFIND 404s; subsequent ones return the real listing.
        let home_attempts = std::sync::Arc::new(AtomicUsize::new(0));
        let (base, recorded) = caldav_recorder::spawn_scripted({
            let principal_xml = principal_xml.clone();
            let home_xml = home_xml.clone();
            let home_path = home_path.clone();
            let home_attempts = home_attempts.clone();
            move |method, path| {
                if method == "PROPFIND" && path == "/dav/calendars" {
                    (
                        axum::http::StatusCode::MULTI_STATUS,
                        principal_xml.as_bytes().to_vec(),
                    )
                } else if method == "PROPFIND" && path == home_path {
                    let n = home_attempts.fetch_add(1, Ordering::SeqCst);
                    if n == 0 {
                        (
                            axum::http::StatusCode::NOT_FOUND,
                            b"Mailbox does not exist".to_vec(),
                        )
                    } else {
                        (
                            axum::http::StatusCode::MULTI_STATUS,
                            home_xml.as_bytes().to_vec(),
                        )
                    }
                } else {
                    (axum::http::StatusCode::CREATED, Vec::new())
                }
            }
        })
        .await;
        let mut sess = JmapSession::new(
            "u.name@fastmail.com",
            "fmu1-test-token",
            Some("test-app-pass"),
        );
        sess.caldav_base = base;

        let r1 = add_to_calendar(&sess, TEST_ICS, "uid-wybm", false).await;
        assert!(
            matches!(r1, Err(Error::CalendarDiscoveryFailed(_))),
            "first call must surface discovery failure, got: {r1:?}"
        );
        let r2 = add_to_calendar(&sess, TEST_ICS, "uid-wybm", false).await;
        assert!(
            r2.is_ok(),
            "second call must succeed (failure was not cached, discovery retried): {r2:?}"
        );

        let rec = recorded.lock().unwrap();
        // Two PROPFINDs per attempt (root + home), two attempts => four total.
        assert_eq!(
            propfind_count(&rec),
            4,
            "discovery must re-run after a failure (two PROPFINDs per attempt): {rec:?}"
        );
        // First attempt failed before PUT; only the second PUTs.
        assert_eq!(
            recorded_by_method(&rec, "PUT").len(),
            1,
            "only the retried (successful) attempt may PUT: {rec:?}"
        );
    }
}
