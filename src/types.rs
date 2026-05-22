use chrono::{DateTime, NaiveDate, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;

// =============================================================================
// Email types
// =============================================================================

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EmailAddress {
    pub name: Option<String>,
    pub email: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Email {
    pub id: String,
    pub blob_id: String,
    pub thread_id: String,
    pub mailbox_ids: HashMap<String, bool>,
    pub keywords: HashMap<String, bool>,
    pub received_at: DateTime<Utc>,
    pub subject: String,
    #[serde(rename = "from")]
    pub from: Vec<EmailAddress>,
    pub to: Vec<EmailAddress>,
    pub cc: Vec<EmailAddress>,
    pub preview: String,
    pub has_attachment: bool,
    pub size: i64,
    pub text_body: Option<String>,
    pub html_body: Option<String>,
    pub has_calendar: bool,
    pub attachments: Vec<Attachment>,
}

impl Email {
    pub fn is_unread(&self) -> bool {
        !self.keywords.contains_key("$seen")
    }

    pub fn is_flagged(&self) -> bool {
        self.keywords.contains_key("$flagged")
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EmailSubmission {
    pub to: Vec<String>,
    pub cc: Vec<String>,
    pub subject: String,
    pub text_body: String,
    pub bcc: Option<Vec<String>>,
    pub html_body: Option<String>,
    pub in_reply_to: Option<String>,
    pub references: Option<Vec<String>>,
    #[serde(default)]
    pub attachments: Vec<Attachment>,
    #[serde(skip)]
    pub calendar_ics: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Mailbox {
    pub id: String,
    pub name: String,
    pub role: Option<String>,
    #[serde(alias = "totalEmails")]
    pub total_emails: i64,
    #[serde(alias = "unreadEmails")]
    pub unread_emails: i64,
    #[serde(alias = "parentId")]
    pub parent_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Identity {
    pub id: String,
    pub email: String,
    pub name: String,
}

// =============================================================================
// Attachment types
// =============================================================================

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Attachment {
    pub blob_id: String,
    pub name: String,
    pub mime_type: String,
    pub size: i64,
}

/// Typed reference to attachment bytes, decoupled from the on-wire string
/// representation each provider uses.
///
/// - `Synthetic` is the compose-flow upload blob: the frontend POSTs bytes to
///   `/api/upload`, gets back a synthetic ID, references it in the draft.
///   Resolved at `send_email` time. Display format: `synth:{uuid}`.
/// - `GmailAttachment` references a Gmail message attachment via the pair
///   `{message_id}:{attachment_id}` — what Gmail's `messages.attachments.get`
///   needs.
///
/// Add provider variants here as they're built (e.g. `OutlookAttachment` for
/// Microsoft Graph). Keeping this typed (instead of a `String` with implicit
/// shape) means provider code can't accidentally feed an upload UUID into the
/// download path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BlobRef {
    Synthetic(uuid::Uuid),
    GmailAttachment { msg_id: String, att_id: String },
}

impl BlobRef {
    /// Generate a fresh synthetic blob reference for a compose-time upload.
    pub fn new_synthetic() -> Self {
        Self::Synthetic(uuid::Uuid::new_v4())
    }

    /// Parse from the wire format used in URLs / Attachment.blob_id strings.
    /// Accepts `synth:{uuid}` or `{msg_id}:{att_id}`.
    ///
    /// Components are validated against URL-safe characters (`[A-Za-z0-9_-]`
    /// plus base64url's `=` padding) so a malformed `blob_id` like
    /// `"../../../../foo:bar"` can't be reflected into a provider HTTP URL
    /// even if it reaches `download_blob` from an untrusted source.
    pub fn parse(s: &str) -> Result<Self, crate::error::Error> {
        // Note: the `synth:` prefix would collide with any Gmail message ID
        // starting with the literal bytes `synth:`. Gmail's actual ID space
        // is URL-safe-base64-ish (hex-leaning), so collision is astronomically
        // unlikely; if it ever happens, the user-visible failure is a clean
        // `BadRequest("invalid synthetic blob UUID")` rather than fetching
        // the wrong attachment. Acceptable; flag here for future maintainers.
        if let Some(rest) = s.strip_prefix("synth:") {
            let uuid = uuid::Uuid::parse_str(rest).map_err(|e| {
                crate::error::Error::BadRequest(format!("invalid synthetic blob UUID: {e}"))
            })?;
            return Ok(Self::Synthetic(uuid));
        }
        match s.split_once(':') {
            Some((msg_id, att_id)) if !msg_id.is_empty() && !att_id.is_empty() => {
                ensure_url_safe(msg_id, "msg_id")?;
                ensure_url_safe(att_id, "att_id")?;
                Ok(Self::GmailAttachment {
                    msg_id: msg_id.to_string(),
                    att_id: att_id.to_string(),
                })
            }
            _ => Err(crate::error::Error::BadRequest(format!(
                "blob_id '{s}' is not a valid BlobRef (expected 'synth:UUID' or 'msg_id:att_id')"
            ))),
        }
    }
}

/// Restrict to URL-path-safe characters: ASCII alphanumerics plus `_`, `-`,
/// and base64url's `=` padding. Gmail message and attachment IDs are within
/// this set; rejecting `/`, `?`, `#`, `..`, etc. prevents a malicious or
/// malformed blob_id from steering the eventual HTTP request to a different
/// API path. Defense-in-depth — `walk_payload` never produces bad input
/// today, but routes that forward client-provided blob_ids might.
fn ensure_url_safe(s: &str, label: &str) -> Result<(), crate::error::Error> {
    if s.chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-' || c == '=')
    {
        Ok(())
    } else {
        Err(crate::error::Error::BadRequest(format!(
            "BlobRef {label} contains characters outside [A-Za-z0-9_=-]"
        )))
    }
}

impl std::fmt::Display for BlobRef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Synthetic(u) => write!(f, "synth:{u}"),
            Self::GmailAttachment { msg_id, att_id } => write!(f, "{msg_id}:{att_id}"),
        }
    }
}

// =============================================================================
// Calendar types
// =============================================================================

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Attendee {
    pub email: String,
    pub name: Option<String>,
    pub status: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CalendarEvent {
    pub uid: String,
    pub summary: String,
    pub dtstart: DateTime<Utc>,
    pub dtend: Option<DateTime<Utc>>,
    pub location: Option<String>,
    pub description: Option<String>,
    pub organizer_email: String,
    pub organizer_name: Option<String>,
    pub attendees: Vec<Attendee>,
    pub sequence: i32,
    pub method: String,
    pub raw_ics: String,
    #[serde(skip_deserializing)]
    pub user_rsvp_status: Option<String>,
}

// =============================================================================
// RSVP types
// =============================================================================

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub enum RsvpStatus {
    #[serde(rename = "ACCEPTED")]
    Accepted,
    #[serde(rename = "TENTATIVE")]
    Tentative,
    #[serde(rename = "DECLINED")]
    Declined,
}

impl RsvpStatus {
    pub fn as_ics_str(&self) -> &'static str {
        match self {
            Self::Accepted => "ACCEPTED",
            Self::Tentative => "TENTATIVE",
            Self::Declined => "DECLINED",
        }
    }
}

// =============================================================================
// Search types
// =============================================================================

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ParsedQuery {
    pub from: Vec<String>,
    pub to: Vec<String>,
    pub subject: Vec<String>,
    pub has_attachment: bool,
    pub is_unread: Option<bool>,
    pub is_flagged: Option<bool>,
    pub before: Option<NaiveDate>,
    pub after: Option<NaiveDate>,
    pub text: String,
}

impl ParsedQuery {
    pub fn is_empty(&self) -> bool {
        self.from.is_empty()
            && self.to.is_empty()
            && self.subject.is_empty()
            && !self.has_attachment
            && self.is_unread.is_none()
            && self.is_flagged.is_none()
            && self.before.is_none()
            && self.after.is_none()
            && self.text.is_empty()
    }
}

// =============================================================================
// Split inbox types
// =============================================================================

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FilterType {
    From,
    To,
    Subject,
    Header,
    Calendar,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum MatchMode {
    #[default]
    Any,
    All,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SplitFilter {
    #[serde(rename = "type")]
    pub filter_type: FilterType,
    pub pattern: String,
    pub name: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SplitInbox {
    pub id: String,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub icon: Option<String>,
    #[serde(default)]
    pub filters: Vec<SplitFilter>,
    #[serde(default)]
    pub match_mode: MatchMode,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SplitsConfig {
    #[serde(default)]
    pub splits: Vec<SplitInbox>,
}

// =============================================================================
// Account error types
// =============================================================================

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AccountError {
    pub account: String,
    pub provider: String,
    pub error: String,
}

// =============================================================================
// App state
// =============================================================================

pub struct AppState {
    pub sessions:
        std::collections::HashMap<String, tokio::sync::RwLock<crate::provider::ProviderSession>>,
    pub account_errors: Vec<AccountError>,
    pub default_account: String,
    pub splits_config_path: PathBuf,
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    fn test_email() -> Email {
        Email {
            id: "test-id".into(),
            blob_id: "blob-id".into(),
            thread_id: "thread-id".into(),
            mailbox_ids: HashMap::new(),
            keywords: HashMap::new(),
            received_at: Utc::now(),
            subject: "Test Subject".into(),
            from: vec![EmailAddress {
                name: None,
                email: "sender@example.com".into(),
            }],
            to: vec![EmailAddress {
                name: None,
                email: "recipient@example.com".into(),
            }],
            cc: vec![],
            preview: "Preview".into(),
            has_attachment: false,
            size: 1000,
            text_body: None,
            html_body: None,
            has_calendar: false,
            attachments: vec![],
        }
    }

    #[test]
    fn email_is_unread_when_no_seen_keyword() {
        let email = test_email();
        assert!(email.is_unread());
    }

    #[test]
    fn email_is_read_when_seen_keyword_present() {
        let mut email = test_email();
        email.keywords.insert("$seen".into(), true);
        assert!(!email.is_unread());
    }

    #[test]
    fn email_is_flagged_when_flagged_keyword_present() {
        let mut email = test_email();
        email.keywords.insert("$flagged".into(), true);
        assert!(email.is_flagged());
    }

    #[test]
    fn email_not_flagged_by_default() {
        let email = test_email();
        assert!(!email.is_flagged());
    }

    #[test]
    fn email_serde_roundtrip() {
        let email = test_email();
        let json = serde_json::to_string(&email).unwrap();
        let deserialized: Email = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized.id, email.id);
        assert_eq!(deserialized.subject, email.subject);
        assert_eq!(deserialized.from[0].email, email.from[0].email);
    }

    #[test]
    fn mailbox_serde_roundtrip() {
        let mailbox = Mailbox {
            id: "mb-1".into(),
            name: "Inbox".into(),
            role: Some("inbox".into()),
            total_emails: 42,
            unread_emails: 5,
            parent_id: None,
        };
        let json = serde_json::to_string(&mailbox).unwrap();
        let deserialized: Mailbox = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized.id, "mb-1");
        assert_eq!(deserialized.role, Some("inbox".into()));
        assert_eq!(deserialized.total_emails, 42);
    }

    #[test]
    fn email_address_serde_roundtrip() {
        let addr = EmailAddress {
            name: Some("Alice".into()),
            email: "alice@example.com".into(),
        };
        let json = serde_json::to_string(&addr).unwrap();
        let deserialized: EmailAddress = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized.name, Some("Alice".into()));
        assert_eq!(deserialized.email, "alice@example.com");
    }

    #[test]
    fn email_submission_with_all_optional_fields() {
        let sub = EmailSubmission {
            to: vec!["a@b.com".into()],
            cc: vec!["c@d.com".into()],
            subject: "Test".into(),
            text_body: "Body".into(),
            bcc: Some(vec!["e@f.com".into()]),
            html_body: Some("<p>Body</p>".into()),
            in_reply_to: Some("msg-123".into()),
            references: Some(vec!["msg-100".into(), "msg-123".into()]),
            attachments: vec![],
            calendar_ics: None,
        };
        let json = serde_json::to_string(&sub).unwrap();
        let deserialized: EmailSubmission = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized.bcc, Some(vec!["e@f.com".into()]));
        assert_eq!(deserialized.in_reply_to, Some("msg-123".into()));
    }

    #[test]
    fn email_submission_with_none_optional_fields() {
        let sub = EmailSubmission {
            to: vec!["a@b.com".into()],
            cc: vec![],
            subject: "Test".into(),
            text_body: "Body".into(),
            bcc: None,
            html_body: None,
            in_reply_to: None,
            references: None,
            attachments: vec![],
            calendar_ics: None,
        };
        let json = serde_json::to_string(&sub).unwrap();
        let deserialized: EmailSubmission = serde_json::from_str(&json).unwrap();
        assert!(deserialized.bcc.is_none());
        assert!(deserialized.in_reply_to.is_none());
    }

    #[test]
    fn filter_type_serializes_to_lowercase() {
        assert_eq!(
            serde_json::to_string(&FilterType::From).unwrap(),
            "\"from\""
        );
        assert_eq!(serde_json::to_string(&FilterType::To).unwrap(), "\"to\"");
        assert_eq!(
            serde_json::to_string(&FilterType::Subject).unwrap(),
            "\"subject\""
        );
        assert_eq!(
            serde_json::to_string(&FilterType::Header).unwrap(),
            "\"header\""
        );
        assert_eq!(
            serde_json::to_string(&FilterType::Calendar).unwrap(),
            "\"calendar\""
        );
    }

    #[test]
    fn match_mode_serializes_to_lowercase() {
        assert_eq!(serde_json::to_string(&MatchMode::Any).unwrap(), "\"any\"");
        assert_eq!(serde_json::to_string(&MatchMode::All).unwrap(), "\"all\"");
    }

    #[test]
    fn split_filter_from_json_all_fields() {
        let json = r#"{"type": "header", "pattern": "calendar", "name": "Content-Type"}"#;
        let filter: SplitFilter = serde_json::from_str(json).unwrap();
        assert_eq!(filter.filter_type, FilterType::Header);
        assert_eq!(filter.pattern, "calendar");
        assert_eq!(filter.name, Some("Content-Type".into()));
    }

    #[test]
    fn split_filter_from_json_no_name() {
        let json = r#"{"type": "from", "pattern": "*@example.com"}"#;
        let filter: SplitFilter = serde_json::from_str(json).unwrap();
        assert_eq!(filter.filter_type, FilterType::From);
        assert!(filter.name.is_none());
    }

    #[test]
    fn split_inbox_default_match_mode() {
        let json = r#"{"id": "test", "name": "Test"}"#;
        let split: SplitInbox = serde_json::from_str(json).unwrap();
        assert_eq!(split.match_mode, MatchMode::Any);
    }

    #[test]
    fn split_inbox_default_filters_empty() {
        let json = r#"{"id": "test", "name": "Test"}"#;
        let split: SplitInbox = serde_json::from_str(json).unwrap();
        assert!(split.filters.is_empty());
    }

    #[test]
    fn split_inbox_icon_defaults_to_none() {
        let json = r#"{"id": "test", "name": "Test"}"#;
        let split: SplitInbox = serde_json::from_str(json).unwrap();
        assert!(split.icon.is_none());
    }

    #[test]
    fn split_inbox_icon_from_json() {
        let json = r#"{"id": "gmail", "name": "Gmail", "icon": "https://cdn.jsdelivr.net/gh/walkxcode/dashboard-icons/svg/gmail.svg"}"#;
        let split: SplitInbox = serde_json::from_str(json).unwrap();
        assert_eq!(
            split.icon.as_deref(),
            Some("https://cdn.jsdelivr.net/gh/walkxcode/dashboard-icons/svg/gmail.svg")
        );
    }

    #[test]
    fn split_inbox_icon_none_omitted_from_json() {
        let split = SplitInbox {
            id: "test".into(),
            name: "Test".into(),
            icon: None,
            filters: vec![],
            match_mode: MatchMode::Any,
        };
        let json = serde_json::to_string(&split).unwrap();
        assert!(!json.contains("icon"));
    }

    #[test]
    fn split_inbox_icon_present_in_json() {
        let split = SplitInbox {
            id: "test".into(),
            name: "Test".into(),
            icon: Some("https://example.com/icon.svg".into()),
            filters: vec![],
            match_mode: MatchMode::Any,
        };
        let json = serde_json::to_string(&split).unwrap();
        assert!(json.contains(r#""icon":"https://example.com/icon.svg""#));
    }

    // --- CalendarEvent tests ---

    #[test]
    fn calendar_event_user_rsvp_status_serializes() {
        let event = CalendarEvent {
            uid: "uid@example.com".into(),
            summary: "Test".into(),
            dtstart: Utc::now(),
            dtend: None,
            location: None,
            description: None,
            organizer_email: "org@example.com".into(),
            organizer_name: None,
            attendees: vec![],
            sequence: 0,
            method: "REQUEST".into(),
            raw_ics: String::new(),
            user_rsvp_status: Some("ACCEPTED".into()),
        };
        let json = serde_json::to_string(&event).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed["user_rsvp_status"], "ACCEPTED");
    }

    #[test]
    fn calendar_event_user_rsvp_status_skip_deserializing() {
        // user_rsvp_status in JSON should be ignored during deserialization
        let json = r#"{
            "uid": "uid@example.com",
            "summary": "Test",
            "dtstart": "2026-02-15T10:00:00Z",
            "dtend": null,
            "location": null,
            "description": null,
            "organizer_email": "org@example.com",
            "organizer_name": null,
            "attendees": [],
            "sequence": 0,
            "method": "REQUEST",
            "raw_ics": "",
            "user_rsvp_status": "ACCEPTED"
        }"#;
        let event: CalendarEvent = serde_json::from_str(json).unwrap();
        assert!(
            event.user_rsvp_status.is_none(),
            "user_rsvp_status should be skipped during deserialization"
        );
    }

    #[test]
    fn calendar_event_user_rsvp_status_none_serializes_as_null() {
        let event = CalendarEvent {
            uid: "uid@example.com".into(),
            summary: "Test".into(),
            dtstart: Utc::now(),
            dtend: None,
            location: None,
            description: None,
            organizer_email: "org@example.com".into(),
            organizer_name: None,
            attendees: vec![],
            sequence: 0,
            method: "REQUEST".into(),
            raw_ics: String::new(),
            user_rsvp_status: None,
        };
        let json = serde_json::to_string(&event).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert!(parsed["user_rsvp_status"].is_null());
    }

    // --- RsvpStatus tests ---

    #[test]
    fn rsvp_status_deserializes_accepted() {
        let status: RsvpStatus = serde_json::from_str("\"ACCEPTED\"").unwrap();
        assert_eq!(status, RsvpStatus::Accepted);
    }

    #[test]
    fn rsvp_status_deserializes_tentative() {
        let status: RsvpStatus = serde_json::from_str("\"TENTATIVE\"").unwrap();
        assert_eq!(status, RsvpStatus::Tentative);
    }

    #[test]
    fn rsvp_status_deserializes_declined() {
        let status: RsvpStatus = serde_json::from_str("\"DECLINED\"").unwrap();
        assert_eq!(status, RsvpStatus::Declined);
    }

    #[test]
    fn rsvp_status_rejects_invalid() {
        assert!(serde_json::from_str::<RsvpStatus>("\"BOGUS\"").is_err());
    }

    #[test]
    fn rsvp_status_as_ics_roundtrip() {
        assert_eq!(RsvpStatus::Accepted.as_ics_str(), "ACCEPTED");
        assert_eq!(RsvpStatus::Tentative.as_ics_str(), "TENTATIVE");
        assert_eq!(RsvpStatus::Declined.as_ics_str(), "DECLINED");
    }

    #[test]
    fn splits_config_empty_default() {
        let config = SplitsConfig::default();
        assert!(config.splits.is_empty());
    }

    #[test]
    fn splits_config_serde_roundtrip() {
        let config = SplitsConfig {
            splits: vec![
                SplitInbox {
                    id: "calendar".into(),
                    name: "Calendar".into(),
                    icon: None,
                    filters: vec![
                        SplitFilter {
                            filter_type: FilterType::From,
                            pattern: "*@calendar.google.com".into(),
                            name: None,
                        },
                        SplitFilter {
                            filter_type: FilterType::Subject,
                            pattern: "invite|invitation".into(),
                            name: None,
                        },
                    ],
                    match_mode: MatchMode::All,
                },
                SplitInbox {
                    id: "newsletters".into(),
                    name: "Newsletters".into(),
                    icon: None,
                    filters: vec![SplitFilter {
                        filter_type: FilterType::From,
                        pattern: "noreply@*".into(),
                        name: None,
                    }],
                    match_mode: MatchMode::Any,
                },
            ],
        };
        let json = serde_json::to_string(&config).unwrap();
        let deserialized: SplitsConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized.splits.len(), 2);
        assert_eq!(deserialized.splits[0].id, "calendar");
        assert_eq!(deserialized.splits[0].filters.len(), 2);
        assert_eq!(deserialized.splits[0].match_mode, MatchMode::All);
        assert_eq!(deserialized.splits[1].id, "newsletters");
        assert_eq!(deserialized.splits[1].match_mode, MatchMode::Any);
    }

    #[test]
    fn mailbox_from_jmap_camel_case() {
        let json = r#"{
            "id": "mb-1",
            "name": "Inbox",
            "role": "inbox",
            "totalEmails": 42,
            "unreadEmails": 5,
            "parentId": null
        }"#;
        let mailbox: Mailbox = serde_json::from_str(json).unwrap();
        assert_eq!(mailbox.id, "mb-1");
        assert_eq!(mailbox.total_emails, 42);
        assert_eq!(mailbox.unread_emails, 5);
        assert!(mailbox.parent_id.is_none());
    }

    #[test]
    fn mailbox_from_snake_case_still_works() {
        let mailbox = Mailbox {
            id: "mb-1".into(),
            name: "Inbox".into(),
            role: Some("inbox".into()),
            total_emails: 42,
            unread_emails: 5,
            parent_id: None,
        };
        let json = serde_json::to_string(&mailbox).unwrap();
        let deserialized: Mailbox = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized.total_emails, 42);
        assert_eq!(deserialized.unread_emails, 5);
    }

    #[test]
    fn account_error_serde_roundtrip() {
        let err = AccountError {
            account: "fastmail".into(),
            provider: "fastmail".into(),
            error: "Authentication failed (401)".into(),
        };
        let json = serde_json::to_string(&err).unwrap();
        let deserialized: AccountError = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized.account, "fastmail");
        assert_eq!(deserialized.provider, "fastmail");
        assert_eq!(deserialized.error, "Authentication failed (401)");
    }

    #[test]
    fn account_error_json_has_expected_keys() {
        let err = AccountError {
            account: "work".into(),
            provider: "outlook".into(),
            error: "OAuth flow failed".into(),
        };
        let json = serde_json::to_string(&err).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert!(parsed.get("account").is_some());
        assert!(parsed.get("provider").is_some());
        assert!(parsed.get("error").is_some());
        assert_eq!(parsed["account"], "work");
        assert_eq!(parsed["provider"], "outlook");
    }

    // ---- BlobRef ----

    #[test]
    fn blob_ref_synthetic_roundtrip() {
        let r = BlobRef::new_synthetic();
        let s = r.to_string();
        assert!(s.starts_with("synth:"));
        let parsed = BlobRef::parse(&s).unwrap();
        assert_eq!(parsed, r);
    }

    #[test]
    fn blob_ref_gmail_roundtrip() {
        let r = BlobRef::GmailAttachment {
            msg_id: "1900abc".into(),
            att_id: "ANGjdJ_xyz".into(),
        };
        let s = r.to_string();
        assert_eq!(s, "1900abc:ANGjdJ_xyz");
        let parsed = BlobRef::parse(&s).unwrap();
        assert_eq!(parsed, r);
    }

    #[test]
    fn blob_ref_parse_rejects_empty() {
        assert!(BlobRef::parse("").is_err());
    }

    #[test]
    fn blob_ref_parse_rejects_no_separator() {
        assert!(BlobRef::parse("just-a-string").is_err());
    }

    #[test]
    fn blob_ref_parse_rejects_empty_components() {
        assert!(BlobRef::parse(":att-id").is_err());
        assert!(BlobRef::parse("msg-id:").is_err());
    }

    #[test]
    fn blob_ref_parse_rejects_bad_synth_uuid() {
        assert!(BlobRef::parse("synth:not-a-uuid").is_err());
    }

    #[test]
    fn blob_ref_synth_prefix_takes_precedence() {
        // A synth: prefix is always Synthetic, even if the rest also contains ':'
        let r = BlobRef::new_synthetic();
        let parsed = BlobRef::parse(&r.to_string()).unwrap();
        assert!(matches!(parsed, BlobRef::Synthetic(_)));
    }

    // ---- BlobRef::parse URL-safety hardening (roborev 174 finding #3) ----

    #[test]
    fn blob_ref_parse_rejects_path_traversal() {
        assert!(BlobRef::parse("../../etc/passwd:foo").is_err());
        assert!(BlobRef::parse("msgid:..%2Fbadpath").is_err());
    }

    #[test]
    fn blob_ref_parse_rejects_slash() {
        assert!(BlobRef::parse("msg/id:att").is_err());
        assert!(BlobRef::parse("msg:att/extra").is_err());
    }

    #[test]
    fn blob_ref_parse_rejects_url_special_chars() {
        assert!(BlobRef::parse("msg?id:att").is_err());
        assert!(BlobRef::parse("msg#id:att").is_err());
        assert!(BlobRef::parse("msg id:att").is_err()); // space
        assert!(BlobRef::parse("msg&id:att").is_err());
    }

    #[test]
    fn blob_ref_parse_accepts_base64url_components() {
        // Gmail IDs use URL-safe base64-ish strings; these must work.
        let r = BlobRef::parse("190abc-DEF_123=:ANGjdJ_xyz0-Q").unwrap();
        assert!(matches!(r, BlobRef::GmailAttachment { .. }));
    }
}
