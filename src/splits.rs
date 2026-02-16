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

pub fn save_splits(config: &SplitsConfig, config_path: &Path) -> Result<(), Error> {
    if let Some(parent) = config_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let json = serde_json::to_string_pretty(config)?;
    std::fs::write(config_path, json)?;
    Ok(())
}

// =============================================================================
// Auto-seed from identities
// =============================================================================

/// Generate identity-based splits from JMAP identities, grouped by domain.
/// Returns None if splits already exist (non-empty config file).
pub fn seed_from_identities(
    identities: &[crate::types::Identity],
    config_path: &Path,
) -> Option<SplitsConfig> {
    // Don't overwrite existing splits
    let existing = load_splits(config_path, None);
    if !existing.splits.is_empty() {
        return None;
    }

    let config = generate_splits_from_identities(identities);
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
pub fn generate_splits_from_identities(identities: &[crate::types::Identity]) -> SplitsConfig {
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
            }],
        };
        let result = filter_by_split(emails, "primary", &config);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].from[0].email, "friend@gmail.com");
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
                },
                SplitInbox {
                    id: "b".into(),
                    name: "B".into(),
                    icon: None,
                    filters: vec![subject_filter("test")],
                    match_mode: MatchMode::All,
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
        let config = generate_splits_from_identities(&identities);
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
        let config = generate_splits_from_identities(&identities);
        assert!(config.splits.is_empty());
    }

    #[test]
    fn generate_splits_empty_identities_returns_empty() {
        let config = generate_splits_from_identities(&[]);
        assert!(config.splits.is_empty());
    }

    #[test]
    fn generate_splits_deduplicates_domains() {
        let identities = vec![
            make_identity("alice@aristoi.ai"),
            make_identity("bob@aristoi.ai"),
            make_identity("user@gmail.com"),
        ];
        let config = generate_splits_from_identities(&identities);
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
        let config = generate_splits_from_identities(&identities);
        assert_eq!(config.splits.len(), 2);
        assert_eq!(config.splits[0].filters[0].pattern, "*@aristoi.ai");
        assert_eq!(config.splits[1].filters[0].pattern, "*@gmail.com");
    }

    #[test]
    fn generate_splits_conflicting_short_names_uses_full_domain() {
        let identities = vec![
            make_identity("user@aristoi.ai"),
            make_identity("user@aristoi.com"),
            make_identity("user@gmail.com"),
        ];
        let config = generate_splits_from_identities(&identities);
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
        let config = generate_splits_from_identities(&identities);
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
        let result = seed_from_identities(&identities, &path);
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
            }],
        };
        save_splits(&existing, &path).unwrap();

        let identities = vec![
            make_identity("user@aristoi.ai"),
            make_identity("user@gmail.com"),
        ];
        let result = seed_from_identities(&identities, &path);
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
        let result = seed_from_identities(&identities, &path);
        assert!(result.is_none());
    }
}
