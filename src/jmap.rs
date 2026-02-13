use crate::error::Error;
use crate::types::ParsedQuery;
use crate::types::*;
use std::collections::HashMap;

// =============================================================================
// JMAP Session
// =============================================================================

pub struct JmapSession {
    pub client: reqwest::Client,
    pub username: String,
    pub auth_header: String,
    pub api_url: Option<String>,
    pub account_id: Option<String>,
    pub upload_url: Option<String>,
    pub download_url: Option<String>,
    pub mailbox_cache: HashMap<String, Mailbox>,
    pub identity_id: Option<String>,
    pub identities: Option<Vec<Identity>>,
}

impl JmapSession {
    pub fn new(username: &str, auth_header: &str) -> Self {
        Self {
            client: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(30))
                .build()
                .expect("failed to create HTTP client"),
            username: username.into(),
            auth_header: auth_header.into(),
            api_url: None,
            account_id: None,
            upload_url: None,
            download_url: None,
            mailbox_cache: HashMap::new(),
            identity_id: None,
            identities: None,
        }
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

    let body: serde_json::Value = resp.json().await?;

    s.api_url = body["apiUrl"].as_str().map(String::from);
    s.upload_url = body["uploadUrl"].as_str().map(String::from);
    s.download_url = body["downloadUrl"].as_str().map(String::from);

    // Extract primary account ID
    if let Some(accounts) = body["primaryAccounts"].as_object() {
        s.account_id = accounts
            .get("urn:ietf:params:jmap:mail")
            .and_then(|v| v.as_str())
            .map(String::from);
    }

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

    let resp = s
        .client
        .post(api_url)
        .header("Authorization", &s.auth_header)
        .json(&payload)
        .send()
        .await?;

    if !resp.status().is_success() {
        return Err(Error::Network(format!(
            "JMAP call failed: HTTP {}",
            resp.status()
        )));
    }

    let body: serde_json::Value = resp.json().await?;
    Ok(body)
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

    let list = resp["methodResponses"][0][1]["list"]
        .as_array()
        .ok_or_else(|| Error::Internal("Invalid Mailbox/get response".into()))?;

    let mut mailboxes = Vec::new();
    for item in list {
        mailboxes.push(Mailbox {
            id: item["id"].as_str().unwrap_or_default().into(),
            name: item["name"].as_str().unwrap_or_default().into(),
            role: item["role"].as_str().map(String::from),
            total_emails: item["totalEmails"].as_i64().unwrap_or(0),
            unread_emails: item["unreadEmails"].as_i64().unwrap_or(0),
            parent_id: item["parentId"].as_str().map(String::from),
        });
    }

    Ok(mailboxes)
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

    let list = resp["methodResponses"][0][1]["list"]
        .as_array()
        .ok_or_else(|| Error::Internal("Invalid Identity/get response".into()))?;

    let mut identities = Vec::new();
    for item in list {
        let id = item["id"].as_str().unwrap_or_default().to_string();
        let email = item["email"].as_str().unwrap_or_default().to_string();
        let name = item["name"].as_str().unwrap_or_default().to_string();

        // Set default identity
        if s.identity_id.is_none() {
            s.identity_id = Some(id.clone());
        }

        identities.push(Identity { id, email, name });
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

pub async fn query_emails(
    s: &JmapSession,
    mailbox_id: Option<&str>,
    limit: usize,
    position: usize,
    query: Option<&ParsedQuery>,
) -> Result<Vec<String>, Error> {
    let account_id = s.account_id.as_ref().ok_or(Error::NotConnected)?;

    let filter = crate::search::to_jmap_filter(query, mailbox_id);

    let resp = jmap_call(
        s,
        vec![serde_json::json!([
            "Email/query",
            {
                "accountId": account_id,
                "filter": filter,
                "sort": [{ "property": "receivedAt", "isAscending": false }],
                "limit": limit,
                "position": position
            },
            "0"
        ])],
    )
    .await?;

    let ids = resp["methodResponses"][0][1]["ids"]
        .as_array()
        .ok_or_else(|| Error::Internal("Invalid Email/query response".into()))?
        .iter()
        .filter_map(|v| v.as_str().map(String::from))
        .collect();

    Ok(ids)
}

pub async fn get_emails(
    s: &JmapSession,
    ids: &[String],
    fetch_body: bool,
) -> Result<Vec<Email>, Error> {
    if ids.is_empty() {
        return Ok(vec![]);
    }

    let account_id = s.account_id.as_ref().ok_or(Error::NotConnected)?;

    let mut properties = vec![
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
    ];
    if fetch_body {
        properties.extend_from_slice(&["textBody", "htmlBody", "bodyValues", "bodyStructure"]);
    }

    let resp = jmap_call(
        s,
        vec![serde_json::json!([
            "Email/get",
            {
                "accountId": account_id,
                "ids": ids,
                "properties": properties,
                "fetchHTMLBodyValues": fetch_body,
                "fetchTextBodyValues": fetch_body,
                "maxBodyValueBytes": 1_000_000
            },
            "0"
        ])],
    )
    .await?;

    let list = resp["methodResponses"][0][1]["list"]
        .as_array()
        .ok_or_else(|| Error::Internal("Invalid Email/get response".into()))?;

    let mut emails = Vec::new();
    for item in list {
        let email = parse_jmap_email(item, fetch_body);
        emails.push(email);
    }

    Ok(emails)
}

fn parse_jmap_email(item: &serde_json::Value, fetch_body: bool) -> Email {
    let keywords: HashMap<String, bool> = item["keywords"]
        .as_object()
        .map(|obj| {
            obj.iter()
                .map(|(k, v)| (k.clone(), v.as_bool().unwrap_or(true)))
                .collect()
        })
        .unwrap_or_default();

    let mailbox_ids: HashMap<String, bool> = item["mailboxIds"]
        .as_object()
        .map(|obj| {
            obj.iter()
                .map(|(k, v)| (k.clone(), v.as_bool().unwrap_or(true)))
                .collect()
        })
        .unwrap_or_default();

    let from = parse_addresses(&item["from"]);
    let to = parse_addresses(&item["to"]);
    let cc = parse_addresses(&item["cc"]);

    let received_at = item["receivedAt"]
        .as_str()
        .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
        .map(|dt| dt.with_timezone(&chrono::Utc))
        .unwrap_or_else(chrono::Utc::now);

    let mut text_body = None;
    let mut html_body = None;
    let mut has_calendar = false;

    if fetch_body {
        // Extract body values
        let body_values = &item["bodyValues"];
        if let Some(text_parts) = item["textBody"].as_array()
            && let Some(first) = text_parts.first()
        {
            let part_id = first["partId"].as_str().unwrap_or_default();
            text_body = body_values[part_id]["value"].as_str().map(String::from);
        }
        if let Some(html_parts) = item["htmlBody"].as_array()
            && let Some(first) = html_parts.first()
        {
            let part_id = first["partId"].as_str().unwrap_or_default();
            html_body = body_values[part_id]["value"].as_str().map(String::from);
        }

        // Check for calendar in body structure
        has_calendar = find_calendar_blob_id(&item["bodyStructure"]).is_some();
    }

    Email {
        id: item["id"].as_str().unwrap_or_default().into(),
        blob_id: item["blobId"].as_str().unwrap_or_default().into(),
        thread_id: item["threadId"].as_str().unwrap_or_default().into(),
        mailbox_ids,
        keywords,
        received_at,
        subject: item["subject"].as_str().unwrap_or_default().into(),
        from,
        to,
        cc,
        preview: item["preview"].as_str().unwrap_or_default().into(),
        has_attachment: item["hasAttachment"].as_bool().unwrap_or(false),
        size: item["size"].as_i64().unwrap_or(0),
        text_body,
        html_body,
        has_calendar,
    }
}

fn parse_addresses(value: &serde_json::Value) -> Vec<EmailAddress> {
    value
        .as_array()
        .map(|arr| {
            arr.iter()
                .map(|a| EmailAddress {
                    name: a["name"]
                        .as_str()
                        .filter(|s| !s.is_empty())
                        .map(String::from),
                    email: a["email"].as_str().unwrap_or_default().into(),
                })
                .collect()
        })
        .unwrap_or_default()
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
    let emails = get_emails(s, &[email_id.to_string()], false).await?;
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

async fn move_to_role(s: &JmapSession, email_id: &str, role: &str) -> Result<bool, Error> {
    debug_assert!(!email_id.is_empty(), "email_id must not be empty");
    let account_id = s.account_id.as_ref().ok_or(Error::NotConnected)?;

    let target_mb = s
        .mailbox_cache
        .values()
        .find(|mb| mb.role.as_deref() == Some(role))
        .ok_or_else(|| Error::Internal(format!("No mailbox with role '{role}'")))?;

    let target_id = target_mb.id.clone();

    let resp = jmap_call(
        s,
        vec![serde_json::json!([
            "Email/set",
            {
                "accountId": account_id,
                "update": {
                    email_id: {
                        "mailboxIds": { target_id: true }
                    }
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

    let resp = jmap_call(
        s,
        vec![serde_json::json!([
            "Email/set",
            {
                "accountId": account_id,
                "update": {
                    email_id: {
                        "mailboxIds": { mailbox_id: true }
                    }
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

    let mut updates = serde_json::Map::new();
    for id in email_ids {
        updates.insert(
            id.clone(),
            serde_json::json!({
                "mailboxIds": { &archive_id: true }
            }),
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
                None => return Ok(None),
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
                    None => return Ok(None),
                }
            }
        }
    };

    // Build email create
    let mut email_create: serde_json::Map<String, serde_json::Value> = serde_json::Map::new();
    email_create.insert("from".into(), serde_json::json!([{"email": from_addr}]));
    email_create.insert(
        "to".into(),
        serde_json::json!(
            sub.to
                .iter()
                .map(|e| serde_json::json!({"email": e}))
                .collect::<Vec<_>>()
        ),
    );
    email_create.insert("subject".into(), serde_json::json!(sub.subject));
    email_create.insert(
        "textBody".into(),
        serde_json::json!([{
            "partId": "body",
            "type": "text/plain"
        }]),
    );
    email_create.insert(
        "bodyValues".into(),
        serde_json::json!({
            "body": { "value": sub.text_body }
        }),
    );

    if !sub.cc.is_empty() {
        email_create.insert(
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
        email_create.insert(
            "bcc".into(),
            serde_json::json!(
                bcc.iter()
                    .map(|e| serde_json::json!({"email": e}))
                    .collect::<Vec<_>>()
            ),
        );
    }

    if let Some(ref reply_to) = sub.in_reply_to {
        email_create.insert("inReplyTo".into(), serde_json::json!([reply_to]));
    }

    if let Some(ref refs) = sub.references {
        email_create.insert("references".into(), serde_json::json!(refs));
    }

    // Build envelope
    let mut rcpt_to: Vec<serde_json::Value> = sub
        .to
        .iter()
        .map(|e| serde_json::json!({"email": e}))
        .collect();
    rcpt_to.extend(sub.cc.iter().map(|e| serde_json::json!({"email": e})));
    if let Some(ref bcc) = sub.bcc {
        rcpt_to.extend(bcc.iter().map(|e| serde_json::json!({"email": e})));
    }

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
                    }
                },
                "1"
            ]),
        ],
    )
    .await?;

    // Check for errors
    let email_created = &resp["methodResponses"][0][1]["created"]["draft"];
    if email_created.is_null() {
        let not_created = &resp["methodResponses"][0][1]["notCreated"];
        if !not_created.is_null() {
            tracing::warn!("Email creation failed: {not_created}");
            return Ok(None);
        }
        return Ok(None);
    }

    let submission = &resp["methodResponses"][1][1]["created"]["send"];
    if submission.is_null() {
        tracing::warn!("Email submission failed");
        return Ok(None);
    }

    // Return the email ID
    let email_id = submission["emailId"]
        .as_str()
        .or_else(|| email_created["id"].as_str())
        .map(String::from);

    Ok(email_id)
}

// =============================================================================
// Calendar
// =============================================================================

pub fn find_calendar_blob_id(body_structure: &serde_json::Value) -> Option<String> {
    if body_structure.is_null() {
        return None;
    }

    // Check this part
    let mime_type = body_structure["type"]
        .as_str()
        .unwrap_or_default()
        .to_lowercase();
    let filename = body_structure["name"]
        .as_str()
        .unwrap_or_default()
        .to_lowercase();

    if mime_type == "text/calendar" || filename.ends_with(".ics") {
        return body_structure["blobId"].as_str().map(String::from);
    }

    // Recurse into sub-parts
    if let Some(parts) = body_structure["subParts"].as_array() {
        for part in parts {
            if let Some(blob_id) = find_calendar_blob_id(part) {
                return Some(blob_id);
            }
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

    let body_structure = &list[0]["bodyStructure"];
    let blob_id = match find_calendar_blob_id(body_structure) {
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
        .client
        .get(&url)
        .header("Authorization", &s.auth_header)
        .send()
        .await?;

    if !resp.status().is_success() {
        return Ok(None);
    }

    let ics_data = resp.text().await?;
    Ok(Some(ics_data))
}

pub async fn add_to_calendar(s: &JmapSession, ics_data: &str) -> Result<bool, Error> {
    // CalDAV PUT to Fastmail calendar
    let caldav_url = format!(
        "https://caldav.fastmail.com/dav/calendars/user/{}/Default/",
        s.username
    );

    // Generate a unique event filename
    let event_id = format!("{}.ics", uuid_v4());

    let resp = s
        .client
        .put(format!("{caldav_url}{event_id}"))
        .header("Authorization", &s.auth_header)
        .header("Content-Type", "text/calendar; charset=utf-8")
        .body(ics_data.to_string())
        .send()
        .await?;

    Ok(resp.status().is_success())
}

/// UUID v4 generation using /dev/urandom for proper randomness.
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

#[cfg(test)]
mod tests {
    use super::*;

    // --- find_calendar_blob_id tests ---

    #[test]
    fn detect_text_calendar_mime() {
        let body = serde_json::json!({
            "type": "text/calendar",
            "blobId": "blob-cal-1"
        });
        assert_eq!(find_calendar_blob_id(&body), Some("blob-cal-1".into()));
    }

    #[test]
    fn detect_ics_filename() {
        let body = serde_json::json!({
            "type": "application/octet-stream",
            "name": "invite.ics",
            "blobId": "blob-cal-2"
        });
        assert_eq!(find_calendar_blob_id(&body), Some("blob-cal-2".into()));
    }

    #[test]
    fn detect_nested_calendar() {
        let body = serde_json::json!({
            "type": "multipart/alternative",
            "subParts": [
                { "type": "text/plain", "blobId": "blob-text" },
                { "type": "text/calendar", "blobId": "blob-cal-3" }
            ]
        });
        assert_eq!(find_calendar_blob_id(&body), Some("blob-cal-3".into()));
    }

    #[test]
    fn no_calendar_returns_none() {
        let body = serde_json::json!({
            "type": "multipart/mixed",
            "subParts": [
                { "type": "text/plain", "blobId": "blob-text" },
                { "type": "text/html", "blobId": "blob-html" }
            ]
        });
        assert_eq!(find_calendar_blob_id(&body), None);
    }

    #[test]
    fn null_body_returns_none() {
        assert_eq!(find_calendar_blob_id(&serde_json::Value::Null), None);
    }

    #[test]
    fn empty_object_returns_none() {
        assert_eq!(find_calendar_blob_id(&serde_json::json!({})), None);
    }

    #[test]
    fn top_level_calendar() {
        let body = serde_json::json!({
            "type": "text/calendar",
            "blobId": "blob-top"
        });
        assert_eq!(find_calendar_blob_id(&body), Some("blob-top".into()));
    }

    #[test]
    fn case_insensitive_mime() {
        let body = serde_json::json!({
            "type": "Text/Calendar",
            "blobId": "blob-case"
        });
        assert_eq!(find_calendar_blob_id(&body), Some("blob-case".into()));
    }

    #[test]
    fn case_insensitive_filename() {
        let body = serde_json::json!({
            "type": "application/octet-stream",
            "name": "Meeting.ICS",
            "blobId": "blob-case-file"
        });
        assert_eq!(find_calendar_blob_id(&body), Some("blob-case-file".into()));
    }
}
