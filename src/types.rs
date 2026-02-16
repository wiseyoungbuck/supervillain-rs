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
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Mailbox {
    pub id: String,
    pub name: String,
    pub role: Option<String>,
    pub total_emails: i64,
    pub unread_emails: i64,
    pub parent_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Identity {
    pub id: String,
    pub email: String,
    pub name: String,
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
// App state
// =============================================================================

pub struct AppState {
    pub session: tokio::sync::RwLock<crate::jmap::JmapSession>,
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
}
