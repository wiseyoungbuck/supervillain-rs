//! Split inbox filters.
//!
//! `splits.json` is a single file, but each `SplitInbox` may carry an
//! `account` tag (a config-section id, e.g. "aristoi"). Tagged splits
//! exist only for that account; untagged splits apply to every account.
//! Route handlers scope the loaded config with [`SplitsConfig::scoped_to`]
//! before filtering or counting, so the synthetic "primary" split means
//! "not matching any of *this account's* splits". A split tagged to a
//! since-deleted account is never listed but stays in the file for
//! hand-editing.
//!
//! Filters run against parsed `Email` objects after fetch, so the same
//! definition works identically on Fastmail, Outlook, and Gmail.
//!
//! Auto-seeding (see [`seed_from_identities`]) runs ONCE at startup
//! against the **default account's** identities, only when `splits.json`
//! is empty, and tags every generated split with that account. It
//! deliberately does not re-run when accounts are added later: doing so
//! would silently clobber the user's edits.

use crate::error::Error;
use crate::glob::glob_match;
use crate::types::*;
use std::path::Path;

// =============================================================================
// Config load/save
// =============================================================================

pub fn load_splits(config_path: &Path, env_override: Option<&str>) -> SplitsConfig {
    // Env var takes precedence
    if let Some(json_str) = env_override {
        return serde_json::from_str(json_str).unwrap_or_default();
    }
    // Try file
    if config_path.exists() {
        let content = match std::fs::read_to_string(config_path) {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!("Failed to read splits config: {e}");
                return SplitsConfig::default();
            }
        };
        return serde_json::from_str(&content).unwrap_or_default();
    }
    SplitsConfig::default()
}

/// Strict variant for startup validation: reports parse/IO errors instead of
/// silently falling back to default. Returns `Ok(None)` when the file is
/// missing (a missing file is normal on first run, not an error). The
/// route-handler path keeps using `load_splits` so a transient read failure
/// never 500s a live request.
pub fn try_load_splits(config_path: &Path) -> Result<Option<SplitsConfig>, String> {
    if !config_path.exists() {
        return Ok(None);
    }
    let content = std::fs::read_to_string(config_path).map_err(|e| format!("read failed: {e}"))?;
    serde_json::from_str::<SplitsConfig>(&content)
        .map(Some)
        .map_err(|e| format!("JSON parse failed: {e}"))
}

pub fn save_splits(config: &SplitsConfig, config_path: &Path) -> Result<(), Error> {
    if let Some(parent) = config_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let json = serde_json::to_string_pretty(config)?;
    std::fs::write(config_path, json)?;
    Ok(())
}

// =============================================================================
// Account scoping
// =============================================================================

impl SplitsConfig {
    /// Splits visible to `account`: untagged splits (visible everywhere)
    /// plus splits tagged with exactly this account. `None` returns the
    /// full config — the management view. Consumes `self`: every caller
    /// already owns the config it's scoping (freshly loaded or cloned),
    /// so there's no reason to force a second clone here.
    pub fn scoped_to(mut self, account: Option<&str>) -> SplitsConfig {
        if let Some(account) = account {
            self.splits
                .retain(|s| s.account.as_deref().is_none_or(|a| a == account));
        }
        self
    }
}

// =============================================================================
// Auto-seed from identities
// =============================================================================

/// Generate identity-based splits from JMAP identities, grouped by domain.
/// Returns None if splits already exist (non-empty config file).
pub fn seed_from_identities(
    identities: &[crate::types::Identity],
    account: &str,
    config_path: &Path,
) -> Option<SplitsConfig> {
    // Don't overwrite existing splits
    let existing = load_splits(config_path, None);
    if !existing.splits.is_empty() {
        return None;
    }

    let config = generate_splits_from_identities(identities, account);
    if config.splits.is_empty() {
        return None;
    }

    if let Err(e) = save_splits(&config, config_path) {
        tracing::warn!("Failed to save auto-generated splits: {e}");
        return None;
    }
    Some(config)
}

/// Generate split tabs from identities, one per unique domain.
/// Skips if there's only one domain (no point in splitting).
pub fn generate_splits_from_identities(
    identities: &[crate::types::Identity],
    account: &str,
) -> SplitsConfig {
    use std::collections::BTreeSet;

    // Collect unique domains
    let mut domains = BTreeSet::new();
    for id in identities {
        if let Some(domain) = id.email.split('@').nth(1) {
            domains.insert(domain.to_lowercase());
        }
    }

    if domains.len() <= 1 {
        return SplitsConfig::default();
    }

    // Check if short names (first label) are unique
    let short_names_unique = {
        let mut seen = std::collections::HashSet::new();
        domains
            .iter()
            .all(|d| seen.insert(d.split('.').next().unwrap_or(d)))
    };

    let splits = domains
        .into_iter()
        .map(|domain| {
            let short = domain.split('.').next().unwrap_or(&domain);
            let (id, name) = if short_names_unique {
                (short.to_string(), short.to_string())
            } else {
                (domain.replace('.', "-"), domain.clone())
            };
            SplitInbox {
                id,
                name,
                icon: None,
                filters: vec![SplitFilter {
                    filter_type: FilterType::To,
                    pattern: format!("*@{domain}"),
                    name: None,
                }],
                match_mode: MatchMode::Any,
                account: Some(account.to_string()),
            }
        })
        .collect();

    SplitsConfig { splits }
}

// =============================================================================
// Filter matching
// =============================================================================

pub fn matches_filter(email: &Email, filter: &SplitFilter) -> bool {
    match filter.filter_type {
        FilterType::From => email
            .from
            .iter()
            .any(|addr| glob_match(&filter.pattern, &addr.email)),
        FilterType::To => {
            let all_recipients = email.to.iter().chain(email.cc.iter());
            all_recipients
                .into_iter()
                .any(|addr| glob_match(&filter.pattern, &addr.email))
        }
        FilterType::Subject => {
            let pattern_lower = filter.pattern.to_lowercase();
            let subject_lower = email.subject.to_lowercase();
            match regex::Regex::new(&format!("(?i){}", filter.pattern)) {
                Ok(re) => re.is_match(&email.subject),
                Err(_) => {
                    tracing::warn!(
                        "Invalid regex '{}', falling back to substring match",
                        filter.pattern
                    );
                    subject_lower.contains(&pattern_lower)
                }
            }
        }
        FilterType::Calendar | FilterType::Header => email.has_calendar,
    }
}

pub fn matches_split(email: &Email, split: &SplitInbox) -> bool {
    if split.filters.is_empty() {
        return false;
    }
    match split.match_mode {
        MatchMode::Any => split.filters.iter().any(|f| matches_filter(email, f)),
        MatchMode::All => split.filters.iter().all(|f| matches_filter(email, f)),
    }
}

pub fn matches_any_split(email: &Email, config: &SplitsConfig) -> bool {
    config
        .splits
        .iter()
        .any(|split| matches_split(email, split))
}

pub fn filter_by_split(emails: Vec<Email>, split_id: &str, config: &SplitsConfig) -> Vec<Email> {
    debug_assert!(!split_id.is_empty(), "split_id must not be empty");

    if split_id == "primary" {
        return emails
            .into_iter()
            .filter(|e| !matches_any_split(e, config))
            .collect();
    }

    let split = match config.splits.iter().find(|s| s.id == split_id) {
        Some(s) => s,
        None => return vec![],
    };

    emails
        .into_iter()
        .filter(|e| matches_split(e, split))
        .collect()
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use std::collections::HashMap;

    fn make_email(from_email: &str, subject: &str) -> Email {
        Email {
            id: "test-id".into(),
            blob_id: "blob-id".into(),
            thread_id: "thread-id".into(),
            mailbox_ids: HashMap::new(),
            keywords: HashMap::new(),
            received_at: Utc::now(),
            subject: subject.into(),
            from: vec![EmailAddress {
                name: None,
                email: from_email.into(),
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
            in_reply_to: None,
        }
    }

    fn make_email_with_to(from: &str, to: &str, cc: &[&str]) -> Email {
        let mut email = make_email(from, "Test");
        email.to = vec![EmailAddress {
            name: None,
            email: to.into(),
        }];
        email.cc = cc
            .iter()
            .map(|e| EmailAddress {
                name: None,
                email: (*e).into(),
            })
            .collect();
        email
    }

    fn from_filter(pattern: &str) -> SplitFilter {
        SplitFilter {
            filter_type: FilterType::From,
            pattern: pattern.into(),
            name: None,
        }
    }

    fn subject_filter(pattern: &str) -> SplitFilter {
        SplitFilter {
            filter_type: FilterType::Subject,
            pattern: pattern.into(),
            name: None,
        }
    }

    fn to_filter(pattern: &str) -> SplitFilter {
        SplitFilter {
            filter_type: FilterType::To,
            pattern: pattern.into(),
            name: None,
        }
    }

    fn tagged_split(id: &str, pattern: &str, account: Option<&str>) -> SplitInbox {
        SplitInbox {
            id: id.into(),
            name: id.into(),
            icon: None,
            filters: vec![to_filter(pattern)],
            match_mode: MatchMode::Any,
            account: account.map(String::from),
        }
    }

    // --- FROM filter ---

    #[test]
    fn from_filter_glob_match() {
        let email = make_email("user@calendar.google.com", "Test");
        assert!(matches_filter(
            &email,
            &from_filter("*@calendar.google.com")
        ));
    }

    #[test]
    fn from_filter_wildcard_domain() {
        let email = make_email("user@calendar.google.com", "Test");
        assert!(matches_filter(
            &email,
            &from_filter("*@calendar.google.com")
        ));
    }

    #[test]
    fn from_filter_wildcard_user() {
        let email = make_email("noreply@anything.com", "Test");
        assert!(matches_filter(&email, &from_filter("noreply@*")));
    }

    #[test]
    fn from_filter_case_insensitive() {
        let email = make_email("User@Calendar.Google.Com", "Test");
        assert!(matches_filter(
            &email,
            &from_filter("*@calendar.google.com")
        ));
    }

    #[test]
    fn from_filter_no_match() {
        let email = make_email("user@other.com", "Test");
        assert!(!matches_filter(
            &email,
            &from_filter("*@calendar.google.com")
        ));
    }

    // --- SUBJECT filter ---

    #[test]
    fn subject_filter_regex_match() {
        let email = make_email("sender@example.com", "Meeting invitation for team");
        assert!(matches_filter(
            &email,
            &subject_filter("invite|invitation|meeting")
        ));
    }

    #[test]
    fn subject_filter_case_insensitive() {
        let email = make_email("sender@example.com", "MEETING INVITATION");
        assert!(matches_filter(
            &email,
            &subject_filter("invite|invitation|meeting")
        ));
    }

    #[test]
    fn subject_filter_invalid_regex_falls_back_to_substring() {
        let email = make_email("sender@example.com", "Test [bracket] text");
        assert!(matches_filter(&email, &subject_filter("[bracket")));
    }

    #[test]
    fn subject_filter_no_match() {
        let email = make_email("sender@example.com", "Nothing relevant here");
        assert!(!matches_filter(
            &email,
            &subject_filter("invite|invitation|meeting")
        ));
    }

    // --- TO filter ---

    #[test]
    fn to_filter_exact_match() {
        let email = make_email_with_to("sender@x.com", "user@example.com", &[]);
        assert!(matches_filter(&email, &to_filter("user@example.com")));
    }

    #[test]
    fn to_filter_glob_wildcard() {
        let email = make_email_with_to("sender@x.com", "user@example.com", &[]);
        assert!(matches_filter(&email, &to_filter("*@example.com")));
    }

    #[test]
    fn to_filter_matches_cc() {
        let email = make_email_with_to("sender@x.com", "other@x.com", &["user@example.com"]);
        assert!(matches_filter(&email, &to_filter("*@example.com")));
    }

    #[test]
    fn to_filter_case_insensitive() {
        let email = make_email_with_to("sender@x.com", "USER@EXAMPLE.COM", &[]);
        assert!(matches_filter(&email, &to_filter("*@example.com")));
    }

    // --- CALENDAR filter ---

    #[test]
    fn calendar_filter_matches_has_calendar() {
        let mut email = make_email("sender@x.com", "Invite");
        email.has_calendar = true;
        let filter = SplitFilter {
            filter_type: FilterType::Calendar,
            pattern: String::new(),
            name: None,
        };
        assert!(matches_filter(&email, &filter));
    }

    #[test]
    fn calendar_filter_no_match_without_calendar() {
        let email = make_email("sender@x.com", "Invite");
        let filter = SplitFilter {
            filter_type: FilterType::Calendar,
            pattern: String::new(),
            name: None,
        };
        assert!(!matches_filter(&email, &filter));
    }

    // --- HEADER filter (legacy, same as calendar) ---

    #[test]
    fn header_filter_matches_has_calendar() {
        let mut email = make_email("sender@x.com", "Invite");
        email.has_calendar = true;
        let filter = SplitFilter {
            filter_type: FilterType::Header,
            pattern: "calendar".into(),
            name: Some("Content-Type".into()),
        };
        assert!(matches_filter(&email, &filter));
    }

    // --- matches_split ---

    #[test]
    fn matches_split_any_mode() {
        let email = make_email("user@calendar.google.com", "Something");
        let split = SplitInbox {
            id: "cal".into(),
            name: "Calendar".into(),
            icon: None,
            filters: vec![
                from_filter("*@calendar.google.com"),
                subject_filter("nonexistent-pattern"),
            ],
            match_mode: MatchMode::Any,
            account: None,
        };
        assert!(matches_split(&email, &split));
    }

    #[test]
    fn matches_split_all_mode_requires_all() {
        let email = make_email("user@calendar.google.com", "Something");
        let split = SplitInbox {
            id: "cal".into(),
            name: "Calendar".into(),
            icon: None,
            filters: vec![
                from_filter("*@calendar.google.com"),
                subject_filter("nonexistent-pattern"),
            ],
            match_mode: MatchMode::All,
            account: None,
        };
        assert!(!matches_split(&email, &split));
    }

    // --- filter_by_split ---

    #[test]
    fn filter_by_split_returns_matching() {
        let emails = vec![
            make_email("user@calendar.google.com", "Invite"),
            make_email("friend@gmail.com", "Hello"),
        ];
        let config = SplitsConfig {
            splits: vec![SplitInbox {
                id: "cal".into(),
                name: "Calendar".into(),
                icon: None,
                filters: vec![from_filter("*@calendar.google.com")],
                match_mode: MatchMode::Any,
                account: None,
            }],
        };
        let result = filter_by_split(emails, "cal", &config);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].from[0].email, "user@calendar.google.com");
    }

    #[test]
    fn filter_by_split_primary_returns_non_matching() {
        let emails = vec![
            make_email("user@calendar.google.com", "Invite"),
            make_email("friend@gmail.com", "Hello"),
        ];
        let config = SplitsConfig {
            splits: vec![SplitInbox {
                id: "cal".into(),
                name: "Calendar".into(),
                icon: None,
                filters: vec![from_filter("*@calendar.google.com")],
                match_mode: MatchMode::Any,
                account: None,
            }],
        };
        let result = filter_by_split(emails, "primary", &config);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].from[0].email, "friend@gmail.com");
    }

    // --- scoped_to ---

    #[test]
    fn scoped_to_keeps_own_and_untagged_drops_others() {
        let config = SplitsConfig {
            splits: vec![
                tagged_split("aristoi", "*@aristoi.ai", Some("aristoi")),
                tagged_split("gmail", "*@gmail.com", Some("gmail")),
                tagged_split("calendar", "*@cal.test", None),
            ],
        };
        let ids: Vec<String> = config
            .scoped_to(Some("aristoi"))
            .splits
            .into_iter()
            .map(|s| s.id)
            .collect();
        assert_eq!(ids, ["aristoi", "calendar"]);
    }

    #[test]
    fn scoped_to_none_returns_all() {
        let config = SplitsConfig {
            splits: vec![
                tagged_split("aristoi", "*@aristoi.ai", Some("aristoi")),
                tagged_split("calendar", "*@cal.test", None),
            ],
        };
        assert_eq!(config.scoped_to(None).splits.len(), 2);
    }

    #[test]
    fn scoped_to_unknown_account_keeps_only_untagged() {
        // A split tagged to a since-deleted account is never listed.
        let config = SplitsConfig {
            splits: vec![
                tagged_split("old", "*@old.test", Some("deleted-account")),
                tagged_split("calendar", "*@cal.test", None),
            ],
        };
        let scoped = config.scoped_to(Some("gmail"));
        assert_eq!(scoped.splits.len(), 1);
        assert_eq!(scoped.splits[0].id, "calendar");
    }

    #[test]
    fn primary_with_scoped_config_ignores_other_accounts_splits() {
        // The bug this feature fixes: a *@gmail.com split visible on every
        // account swallowed all mail on the gmail account, emptying Primary
        // — and conversely gmail-bound mail vanished from other accounts'
        // Primary. Scoped away, the split must not claim the mail.
        let emails = vec![make_email_with_to("alice@x.com", "matt@gmail.com", &[])];
        let config = SplitsConfig {
            splits: vec![tagged_split("gmail", "*@gmail.com", Some("gmail"))],
        };
        let scoped = config.scoped_to(Some("aristoi"));
        let primary = filter_by_split(emails, "primary", &scoped);
        assert_eq!(primary.len(), 1);
    }

    // --- matches_any_split ---

    #[test]
    fn matches_any_split_true_when_matching() {
        let email = make_email("user@calendar.google.com", "Invite");
        let config = SplitsConfig {
            splits: vec![SplitInbox {
                id: "cal".into(),
                name: "Calendar".into(),
                icon: None,
                filters: vec![from_filter("*@calendar.google.com")],
                match_mode: MatchMode::Any,
                account: None,
            }],
        };
        assert!(matches_any_split(&email, &config));
    }

    #[test]
    fn matches_any_split_false_with_no_splits() {
        let email = make_email("user@example.com", "Test");
        let config = SplitsConfig::default();
        assert!(!matches_any_split(&email, &config));
    }

    // --- Config load/save ---

    #[test]
    fn load_nonexistent_returns_empty() {
        let config = load_splits(Path::new("/nonexistent/path/splits.json"), None);
        assert!(config.splits.is_empty());
    }

    #[test]
    fn try_load_missing_file_is_ok_none() {
        let result = try_load_splits(Path::new("/nonexistent/path/splits.json"));
        assert!(matches!(result, Ok(None)));
    }

    #[test]
    fn try_load_invalid_json_returns_err() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("splits.json");
        std::fs::write(&path, "{not valid json").unwrap();
        let result = try_load_splits(&path);
        let err = result.expect_err("should reject invalid JSON");
        assert!(err.contains("JSON parse failed"));
    }

    #[test]
    fn try_load_valid_file_returns_config() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("splits.json");
        std::fs::write(
            &path,
            r#"{"splits": [{"id": "x", "name": "X", "filters": [], "match_mode": "any"}]}"#,
        )
        .unwrap();
        let cfg = try_load_splits(&path).unwrap().expect("should parse");
        assert_eq!(cfg.splits.len(), 1);
    }

    #[test]
    fn load_from_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("splits.json");
        let data =
            r#"{"splits": [{"id": "test", "name": "Test", "filters": [], "match_mode": "any"}]}"#;
        std::fs::write(&path, data).unwrap();
        let config = load_splits(&path, None);
        assert_eq!(config.splits.len(), 1);
        assert_eq!(config.splits[0].id, "test");
    }

    #[test]
    fn load_env_takes_precedence() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("splits.json");
        std::fs::write(&path, r#"{"splits": [{"id": "file", "name": "File"}]}"#).unwrap();
        let env_json = r#"{"splits": [{"id": "env", "name": "Env"}]}"#;
        let config = load_splits(&path, Some(env_json));
        assert_eq!(config.splits.len(), 1);
        assert_eq!(config.splits[0].id, "env");
    }

    #[test]
    fn load_complex_config() {
        let json = r#"{
            "splits": [{
                "id": "cal",
                "name": "Calendar",
                "filters": [
                    {"type": "from", "pattern": "*@calendar.google.com"},
                    {"type": "subject", "pattern": "invite|invitation"}
                ],
                "match_mode": "all"
            }]
        }"#;
        let config = load_splits(Path::new("/nonexistent"), Some(json));
        assert_eq!(config.splits[0].filters.len(), 2);
        assert_eq!(config.splits[0].match_mode, MatchMode::All);
    }

    #[test]
    fn save_creates_directory() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("subdir").join("splits.json");
        let config = SplitsConfig::default();
        save_splits(&config, &path).unwrap();
        assert!(path.exists());
    }

    #[test]
    fn save_overwrites_existing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("splits.json");
        std::fs::write(&path, "old content").unwrap();
        let config = SplitsConfig {
            splits: vec![SplitInbox {
                id: "new".into(),
                name: "New".into(),
                icon: None,
                filters: vec![],
                match_mode: MatchMode::Any,
                account: None,
            }],
        };
        save_splits(&config, &path).unwrap();
        let content = std::fs::read_to_string(&path).unwrap();
        assert!(content.contains("\"new\""));
    }

    #[test]
    fn save_preserves_filter_details() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("splits.json");
        let config = SplitsConfig {
            splits: vec![SplitInbox {
                id: "test".into(),
                name: "Test".into(),
                icon: None,
                filters: vec![SplitFilter {
                    filter_type: FilterType::Header,
                    pattern: "calendar".into(),
                    name: Some("Content-Type".into()),
                }],
                match_mode: MatchMode::All,
                account: None,
            }],
        };
        save_splits(&config, &path).unwrap();
        let loaded = load_splits(&path, None);
        assert_eq!(loaded.splits[0].filters[0].filter_type, FilterType::Header);
        assert_eq!(loaded.splits[0].filters[0].pattern, "calendar");
        assert_eq!(
            loaded.splits[0].filters[0].name,
            Some("Content-Type".into())
        );
    }

    #[test]
    fn save_load_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("splits.json");
        let config = SplitsConfig {
            splits: vec![
                SplitInbox {
                    id: "a".into(),
                    name: "A".into(),
                    icon: None,
                    filters: vec![from_filter("*@example.com")],
                    match_mode: MatchMode::Any,
                    account: None,
                },
                SplitInbox {
                    id: "b".into(),
                    name: "B".into(),
                    icon: None,
                    filters: vec![subject_filter("test")],
                    match_mode: MatchMode::All,
                    account: None,
                },
            ],
        };
        save_splits(&config, &path).unwrap();
        let loaded = load_splits(&path, None);
        assert_eq!(loaded.splits.len(), 2);
        assert_eq!(loaded.splits[0].id, "a");
        assert_eq!(loaded.splits[1].id, "b");
    }

    // --- generate_splits_from_identities ---

    fn make_identity(email: &str) -> Identity {
        Identity {
            id: email.into(),
            email: email.into(),
            name: String::new(),
        }
    }

    #[test]
    fn generate_splits_multiple_domains() {
        let identities = vec![
            make_identity("user@aristoi.ai"),
            make_identity("user@gmail.com"),
            make_identity("user@aristotle.ai"),
        ];
        let config = generate_splits_from_identities(&identities, "acct");
        assert_eq!(config.splits.len(), 3);

        // BTreeMap sorts alphabetically by domain
        assert_eq!(config.splits[0].id, "aristoi");
        assert_eq!(config.splits[0].filters[0].pattern, "*@aristoi.ai");
        assert_eq!(config.splits[0].filters[0].filter_type, FilterType::To);

        assert_eq!(config.splits[1].id, "aristotle");
        assert_eq!(config.splits[1].filters[0].pattern, "*@aristotle.ai");

        assert_eq!(config.splits[2].id, "gmail");
        assert_eq!(config.splits[2].filters[0].pattern, "*@gmail.com");
    }

    #[test]
    fn generate_splits_single_domain_returns_empty() {
        let identities = vec![
            make_identity("user@fastmail.com"),
            make_identity("alias@fastmail.com"),
        ];
        let config = generate_splits_from_identities(&identities, "acct");
        assert!(config.splits.is_empty());
    }

    #[test]
    fn generate_splits_empty_identities_returns_empty() {
        let config = generate_splits_from_identities(&[], "acct");
        assert!(config.splits.is_empty());
    }

    #[test]
    fn generate_splits_deduplicates_domains() {
        let identities = vec![
            make_identity("alice@aristoi.ai"),
            make_identity("bob@aristoi.ai"),
            make_identity("user@gmail.com"),
        ];
        let config = generate_splits_from_identities(&identities, "acct");
        assert_eq!(config.splits.len(), 2);
        assert_eq!(config.splits[0].id, "aristoi");
        assert_eq!(config.splits[1].id, "gmail");
    }

    #[test]
    fn generate_splits_case_insensitive_domains() {
        let identities = vec![
            make_identity("user@Aristoi.AI"),
            make_identity("user@Gmail.Com"),
        ];
        let config = generate_splits_from_identities(&identities, "acct");
        assert_eq!(config.splits.len(), 2);
        assert_eq!(config.splits[0].filters[0].pattern, "*@aristoi.ai");
        assert_eq!(config.splits[1].filters[0].pattern, "*@gmail.com");
    }

    #[test]
    fn generated_splits_are_tagged_with_seeding_account() {
        let identities = vec![
            make_identity("user@aristoi.ai"),
            make_identity("user@gmail.com"),
        ];
        let config = generate_splits_from_identities(&identities, "aristoi");
        assert_eq!(config.splits.len(), 2);
        assert!(
            config
                .splits
                .iter()
                .all(|s| s.account.as_deref() == Some("aristoi"))
        );
    }

    #[test]
    fn generate_splits_conflicting_short_names_uses_full_domain() {
        let identities = vec![
            make_identity("user@aristoi.ai"),
            make_identity("user@aristoi.com"),
            make_identity("user@gmail.com"),
        ];
        let config = generate_splits_from_identities(&identities, "acct");
        assert_eq!(config.splits.len(), 3);
        // All should use full domain since "aristoi" collides
        assert_eq!(config.splits[0].id, "aristoi-ai");
        assert_eq!(config.splits[0].name, "aristoi.ai");
        assert_eq!(config.splits[1].id, "aristoi-com");
        assert_eq!(config.splits[2].id, "gmail-com");
    }

    #[test]
    fn generate_splits_skips_malformed_email() {
        let identities = vec![
            make_identity("localonly"),
            make_identity("user@aristoi.ai"),
            make_identity("user@gmail.com"),
        ];
        let config = generate_splits_from_identities(&identities, "acct");
        assert_eq!(config.splits.len(), 2);
    }

    // --- seed_from_identities ---

    #[test]
    fn seed_creates_splits_when_no_config() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("splits.json");
        let identities = vec![
            make_identity("user@aristoi.ai"),
            make_identity("user@gmail.com"),
        ];
        let result = seed_from_identities(&identities, "acct", &path);
        assert!(result.is_some());
        assert_eq!(result.unwrap().splits.len(), 2);

        // File should have been written
        let loaded = load_splits(&path, None);
        assert_eq!(loaded.splits.len(), 2);
    }

    #[test]
    fn seed_skips_when_splits_exist() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("splits.json");

        // Pre-create a splits config
        let existing = SplitsConfig {
            splits: vec![SplitInbox {
                id: "custom".into(),
                name: "Custom".into(),
                icon: None,
                filters: vec![from_filter("*@example.com")],
                match_mode: MatchMode::Any,
                account: None,
            }],
        };
        save_splits(&existing, &path).unwrap();

        let identities = vec![
            make_identity("user@aristoi.ai"),
            make_identity("user@gmail.com"),
        ];
        let result = seed_from_identities(&identities, "acct", &path);
        assert!(result.is_none());

        // Original config should be preserved
        let loaded = load_splits(&path, None);
        assert_eq!(loaded.splits.len(), 1);
        assert_eq!(loaded.splits[0].id, "custom");
    }

    #[test]
    fn seed_skips_single_domain() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("splits.json");
        let identities = vec![make_identity("user@fastmail.com")];
        let result = seed_from_identities(&identities, "acct", &path);
        assert!(result.is_none());
    }

    // --- O365 via FastMail relay ---

    #[test]
    fn generate_splits_fastmail_plus_o365() {
        let identities = vec![
            make_identity("matt@fastmail.com"),
            make_identity("matt@company.onmicrosoft.com"),
        ];
        let config = generate_splits_from_identities(&identities, "acct");
        assert_eq!(config.splits.len(), 2);
        let o365_split = config
            .splits
            .iter()
            .find(|s| s.filters[0].pattern.contains("onmicrosoft.com"));
        assert!(o365_split.is_some());
        assert_eq!(o365_split.unwrap().filters[0].filter_type, FilterType::To);
    }

    #[test]
    fn forwarded_o365_mail_matches_split_by_to() {
        let email = make_email_with_to("sender@external.com", "matt@company.onmicrosoft.com", &[]);
        let split = SplitInbox {
            id: "company".into(),
            name: "Company".into(),
            icon: None,
            filters: vec![to_filter("*@company.onmicrosoft.com")],
            match_mode: MatchMode::Any,
            account: None,
        };
        assert!(matches_split(&email, &split));
    }

    // Documents the known limitation: if O365 rewrites To: to the FastMail
    // address during forwarding, the split filter won't match.
    #[test]
    fn forwarded_o365_mail_rewritten_to_does_not_match() {
        let email = make_email_with_to("sender@external.com", "matt@fastmail.com", &[]);
        let split = SplitInbox {
            id: "company".into(),
            name: "Company".into(),
            icon: None,
            filters: vec![to_filter("*@company.onmicrosoft.com")],
            match_mode: MatchMode::Any,
            account: None,
        };
        assert!(!matches_split(&email, &split));
    }

    #[test]
    fn primary_split_excludes_o365_mail() {
        let emails = vec![
            make_email_with_to("alice@x.com", "matt@company.onmicrosoft.com", &[]),
            make_email_with_to("bob@y.com", "matt@fastmail.com", &[]),
        ];
        let config = SplitsConfig {
            splits: vec![SplitInbox {
                id: "company".into(),
                name: "Company".into(),
                icon: None,
                filters: vec![to_filter("*@company.onmicrosoft.com")],
                match_mode: MatchMode::Any,
                account: None,
            }],
        };
        let primary = filter_by_split(emails, "primary", &config);
        assert_eq!(primary.len(), 1);
        assert_eq!(primary[0].to[0].email, "matt@fastmail.com");
    }
}
