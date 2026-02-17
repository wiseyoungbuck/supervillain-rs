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
        if let Some(text_parts) = item["textBody"].as_array() {
            let parts: Vec<&str> = text_parts
                .iter()
                .filter_map(|p| {
                    let part_id = p["partId"].as_str().unwrap_or_default();
                    body_values[part_id]["value"].as_str()
                })
                .collect();
            if !parts.is_empty() {
                text_body = Some(parts.join("\n"));
            }
        }
        if let Some(html_parts) = item["htmlBody"].as_array() {
            let parts: Vec<&str> = html_parts
                .iter()
                .filter_map(|p| {
                    let part_id = p["partId"].as_str().unwrap_or_default();
                    body_values[part_id]["value"].as_str()
                })
                .collect();
            if !parts.is_empty() {
                html_body = Some(parts.join("\n"));
            }
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
    m.insert(
        "textBody".into(),
        serde_json::json!([{
            "partId": "body",
            "type": "text/plain"
        }]),
    );

    if let Some(ref html) = sub.html_body {
        m.insert(
            "htmlBody".into(),
            serde_json::json!([{
                "partId": "html",
                "type": "text/html"
            }]),
        );
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

    // JMAP requires mailboxIds — put the draft in Drafts
    let drafts_id = s
        .mailbox_cache
        .values()
        .find(|mb| mb.role.as_deref() == Some("drafts"))
        .ok_or_else(|| Error::Internal("No drafts mailbox found".into()))?
        .id
        .clone();

    let email_create = build_draft_email(sub, from_addr, &drafts_id);

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

pub async fn add_to_calendar(
    s: &JmapSession,
    ics_data: &str,
    uid: &str,
    only_if_new: bool,
) -> Result<bool, Error> {
    // CalDAV PUT to Fastmail calendar, using event UID as filename for idempotency
    let caldav_url = format!(
        "https://caldav.fastmail.com/dav/calendars/user/{}/Default/{}.ics",
        s.username, uid
    );

    let mut req = s
        .client
        .put(&caldav_url)
        .header("Authorization", &s.auth_header)
        .header("Content-Type", "text/calendar; charset=utf-8");

    // If-None-Match: * means "only create, don't overwrite existing"
    if only_if_new {
        req = req.header("If-None-Match", "*");
    }

    let resp = req.body(ics_data.to_string()).send().await?;

    Ok(resp.status().is_success())
}

pub async fn remove_from_calendar(s: &JmapSession, uid: &str) -> Result<bool, Error> {
    let caldav_url = format!(
        "https://caldav.fastmail.com/dav/calendars/user/{}/Default/{}.ics",
        s.username, uid
    );

    let resp = s
        .client
        .delete(&caldav_url)
        .header("Authorization", &s.auth_header)
        .send()
        .await?;

    Ok(resp.status().is_success())
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
        let text_body = &draft["textBody"];
        assert_eq!(text_body[0]["type"], "text/plain");
        assert_eq!(text_body[0]["partId"], "body");
        assert_eq!(draft["bodyValues"]["body"]["value"], "Hello");
        // Finding #4: text-only drafts should also have bodyStructure
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
        };
        let draft = build_draft_email(&sub, "alice@example.com", "mb-drafts");
        // Should have htmlBody key with text/html part
        assert!(
            draft.contains_key("htmlBody"),
            "Draft should include htmlBody when html_body is Some"
        );
        let html_body = &draft["htmlBody"];
        assert!(!html_body.is_null(), "htmlBody should not be null");
        // htmlBody entry should have type text/html
        assert_eq!(
            html_body[0]["type"], "text/html",
            "htmlBody part should have type text/html"
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
        // Finding #1: draft should have bodyStructure with multipart/alternative
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
}
