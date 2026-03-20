use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

use crate::error::Error;
use crate::types::CalendarEvent;

// =============================================================================
// Outlook Session
// =============================================================================

pub struct OutlookSession {
    pub client: reqwest::Client,
    pub token: tokio::sync::Mutex<OutlookToken>,
    pub client_id: String,
    pub token_path: PathBuf,
    pub email: String,
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
const SCOPES: &str = "Calendars.ReadWrite offline_access";

// =============================================================================
// OAuth2 PKCE
// =============================================================================

/// Generate a random code verifier for PKCE (43-128 chars, unreserved charset)
fn generate_code_verifier() -> String {
    use rand::Rng;
    const CHARSET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-._~";
    let mut rng = rand::rng();
    (0..64)
        .map(|_| {
            let idx = rng.random_range(0..CHARSET.len());
            CHARSET[idx] as char
        })
        .collect()
}

/// Generate a random state parameter for CSRF protection
fn generate_state() -> String {
    use rand::Rng;
    let mut rng = rand::rng();
    (0..32)
        .map(|_| format!("{:02x}", rng.random_range(0u8..=255)))
        .collect()
}

/// S256 code challenge from verifier
fn code_challenge(verifier: &str) -> String {
    use base64::Engine;
    let digest = sha256(verifier.as_bytes());
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(digest)
}

fn sha256(data: &[u8]) -> [u8; 32] {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(data);
    hasher.finalize().into()
}

/// Build the authorization URL for the OAuth2 PKCE flow
pub fn auth_url(client_id: &str, code_verifier: &str, state: &str) -> String {
    let challenge = code_challenge(code_verifier);
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
    })
}

/// One-shot OAuth2 PKCE flow: opens browser, runs local callback server, exchanges code
pub async fn oauth_flow(
    client_id: &str,
    token_path: &std::path::Path,
) -> Result<OutlookSession, Error> {
    let code_verifier = generate_code_verifier();
    let expected_state = generate_state();
    let url = auth_url(client_id, &code_verifier, &expected_state);

    // Start a one-shot server to receive the callback
    let listener = tokio::net::TcpListener::bind("127.0.0.1:8400")
        .await
        .map_err(|e| Error::Internal(format!("Failed to bind OAuth callback server: {e}")))?;

    eprintln!("\nOpen this URL to authorize Outlook access:\n\n  {url}\n");

    // Try to open browser
    #[cfg(target_os = "macos")]
    {
        let _ = std::process::Command::new("open").arg(&url).spawn();
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = std::process::Command::new("xdg-open").arg(&url).spawn();
    }

    // Wait for the callback with the authorization code
    use axum::{Router, extract::Query, routing::get};
    use tokio::sync::oneshot;

    let (tx, rx) = oneshot::channel::<(String, String)>();
    let tx = std::sync::Arc::new(tokio::sync::Mutex::new(Some(tx)));

    #[derive(Deserialize)]
    struct CallbackParams {
        code: String,
        #[serde(default)]
        state: String,
    }

    let tx_clone = tx.clone();
    let callback_app = Router::new().route(
        "/callback",
        get(move |Query(params): Query<CallbackParams>| {
            let tx = tx_clone.clone();
            async move {
                if let Some(sender) = tx.lock().await.take() {
                    let _ = sender.send((params.code, params.state));
                }
                "Authorization successful! You can close this tab."
            }
        }),
    );

    let server = axum::serve(listener, callback_app);

    // Run server until we get the code
    tokio::select! {
        result = server => {
            result.map_err(|e| Error::Internal(format!("OAuth callback server error: {e}")))?;
            Err(Error::Internal("OAuth callback server exited without receiving code".into()))
        }
        code_and_state = rx => {
            let (code, state) = code_and_state.map_err(|_| Error::Internal("OAuth flow cancelled".into()))?;

            // Validate state parameter to prevent CSRF
            if state != expected_state {
                return Err(Error::Auth("OAuth state mismatch — possible CSRF attack".into()));
            }

            let client = reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(30))
                .build()
                .expect("failed to create HTTP client");

            let token_resp = exchange_code(&client, client_id, &code, &code_verifier).await?;

            // Fetch user's email from Graph
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
            };

            save_tokens(&session)?;
            tracing::info!("Outlook OAuth completed for {}", session.email);
            Ok(session)
        }
    }
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
    fn sha256_empty() {
        let hash = sha256(b"");
        let hex: String = hash.iter().map(|b| format!("{b:02x}")).collect();
        assert_eq!(
            hex,
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }

    #[test]
    fn sha256_hello() {
        let hash = sha256(b"hello");
        let hex: String = hash.iter().map(|b| format!("{b:02x}")).collect();
        assert_eq!(
            hex,
            "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824"
        );
    }

    #[test]
    fn sha256_multiblock() {
        // Message longer than 64 bytes forces multiple SHA-256 blocks
        let data = b"The quick brown fox jumps over the lazy dog. And then some more text to exceed 64 bytes.";
        let hash = sha256(data);
        let hex: String = hash.iter().map(|b| format!("{b:02x}")).collect();
        // Pre-computed via: echo -n '...' | shasum -a 256
        assert_eq!(hex.len(), 64);
        // Verify it's deterministic
        assert_eq!(hash, sha256(data));
    }

    #[test]
    fn code_verifier_length() {
        let v = generate_code_verifier();
        assert_eq!(v.len(), 64);
        // All chars should be unreserved
        assert!(
            v.chars()
                .all(|c| c.is_ascii_alphanumeric() || "-._~".contains(c))
        );
    }

    #[test]
    fn code_challenge_is_base64url() {
        let verifier = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
        let challenge = code_challenge(verifier);
        // Should be base64url without padding
        assert!(!challenge.contains('='));
        assert!(!challenge.contains('+'));
        assert!(!challenge.contains('/'));
        assert!(!challenge.is_empty());
    }

    #[test]
    fn code_challenge_rfc7636_appendix_b() {
        // RFC 7636 Appendix B test vector
        let verifier = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
        let challenge = code_challenge(verifier);
        assert_eq!(challenge, "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM");
    }

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
    fn generate_state_is_hex_and_correct_length() {
        let state = generate_state();
        assert_eq!(state.len(), 64); // 32 bytes * 2 hex chars
        assert!(state.chars().all(|c| c.is_ascii_hexdigit()));
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
}
