use crate::types::ParsedQuery;
use chrono::NaiveDate;

// =============================================================================
// Query parser
// =============================================================================

pub fn parse_query(raw: &str) -> ParsedQuery {
    let mut query = ParsedQuery::default();
    let raw = raw.trim();
    if raw.is_empty() {
        return query;
    }

    let mut free_text_parts: Vec<String> = Vec::new();
    let mut pos = 0;

    while pos < raw.len() {
        // Skip whitespace
        while pos < raw.len() && raw.as_bytes()[pos] == b' ' {
            pos += 1;
        }
        if pos >= raw.len() {
            break;
        }

        // Try to match an operator (keyword:value)
        if let Some(colon_pos) = raw[pos..].find(':') {
            let abs_colon = pos + colon_pos;
            let keyword = &raw[pos..abs_colon];

            // Only recognize known operators (no spaces in keyword)
            if !keyword.contains(' ') && is_known_operator(keyword) {
                let value_start = abs_colon + 1;
                let (value, value_end) = extract_value(raw, value_start);

                match keyword {
                    "from" => query.from.push(value),
                    "to" => query.to.push(value),
                    "subject" => query.subject.push(value),
                    "has" if value == "attachment" => query.has_attachment = true,
                    "is" => match value.as_str() {
                        "unread" => query.is_unread = Some(true),
                        "read" => query.is_unread = Some(false),
                        "starred" | "flagged" => query.is_flagged = Some(true),
                        _ => {}
                    },
                    "before" => query.before = parse_date(&value),
                    "after" => query.after = parse_date(&value),
                    "newer_than" => query.after = parse_date_offset(&value),
                    "older_than" => query.before = parse_date_offset(&value),
                    _ => {}
                }

                pos = value_end;
                continue;
            }
        }

        // Not an operator — collect as free text word
        let word_end = raw[pos..].find(' ').map(|i| pos + i).unwrap_or(raw.len());
        free_text_parts.push(raw[pos..word_end].to_string());
        pos = word_end;
    }

    query.text = free_text_parts.join(" ");
    query
}

fn is_known_operator(keyword: &str) -> bool {
    matches!(
        keyword,
        "from" | "to" | "subject" | "has" | "is" | "before" | "after" | "newer_than" | "older_than"
    )
}

fn extract_value(raw: &str, start: usize) -> (String, usize) {
    if start >= raw.len() {
        return (String::new(), start);
    }

    // Quoted value
    if raw.as_bytes()[start] == b'"' {
        let content_start = start + 1;
        let end = raw[content_start..]
            .find('"')
            .map(|i| content_start + i)
            .unwrap_or(raw.len());
        let value = raw[content_start..end].to_string();
        let past_quote = if end < raw.len() { end + 1 } else { end };
        return (value, past_quote);
    }

    // Unquoted value — up to next space
    let end = raw[start..]
        .find(' ')
        .map(|i| start + i)
        .unwrap_or(raw.len());
    (raw[start..end].to_string(), end)
}

fn parse_date(s: &str) -> Option<NaiveDate> {
    NaiveDate::parse_from_str(s, "%Y-%m-%d").ok()
}

fn parse_date_offset(s: &str) -> Option<NaiveDate> {
    let s = s.trim();
    if s.len() < 2 {
        return None;
    }

    // Try relative format first: Nd, Nw, Nm
    let (num_str, unit) = s.split_at(s.len() - 1);
    if let Ok(num) = num_str.parse::<i64>()
        && num > 0
    {
        let days = match unit {
            "d" => Some(num),
            "w" => Some(num * 7),
            "m" => Some(num * 30),
            _ => None,
        };
        if let Some(days) = days {
            return Some(chrono::Utc::now().date_naive() - chrono::Duration::days(days));
        }
    }

    // Fallback: absolute date MM-DD-YY or MM-DD-YYYY
    NaiveDate::parse_from_str(s, "%m-%d-%y")
        .or_else(|_| NaiveDate::parse_from_str(s, "%m-%d-%Y"))
        .ok()
}

// =============================================================================
// JMAP filter translation
// =============================================================================

pub fn to_jmap_filter(query: Option<&ParsedQuery>, mailbox_id: Option<&str>) -> serde_json::Value {
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

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    // --- Parser tests ---

    #[test]
    fn parse_empty_string() {
        let q = parse_query("");
        assert!(q.is_empty());
    }

    #[test]
    fn parse_from_operator() {
        let q = parse_query("from:john@example.com");
        assert_eq!(q.from, vec!["john@example.com"]);
    }

    #[test]
    fn parse_to_operator() {
        let q = parse_query("to:alice@example.com");
        assert_eq!(q.to, vec!["alice@example.com"]);
    }

    #[test]
    fn parse_subject_operator() {
        let q = parse_query("subject:meeting");
        assert_eq!(q.subject, vec!["meeting"]);
    }

    #[test]
    fn parse_subject_quoted() {
        let q = parse_query("subject:\"hello world\"");
        assert_eq!(q.subject, vec!["hello world"]);
    }

    #[test]
    fn parse_has_attachment() {
        let q = parse_query("has:attachment");
        assert!(q.has_attachment);
    }

    #[test]
    fn parse_is_unread() {
        let q = parse_query("is:unread");
        assert_eq!(q.is_unread, Some(true));
    }

    #[test]
    fn parse_is_read() {
        let q = parse_query("is:read");
        assert_eq!(q.is_unread, Some(false));
    }

    #[test]
    fn parse_is_starred() {
        let q = parse_query("is:starred");
        assert_eq!(q.is_flagged, Some(true));
    }

    #[test]
    fn parse_is_flagged() {
        let q = parse_query("is:flagged");
        assert_eq!(q.is_flagged, Some(true));
    }

    #[test]
    fn parse_before_date() {
        let q = parse_query("before:2026-01-15");
        assert_eq!(
            q.before,
            Some(NaiveDate::from_ymd_opt(2026, 1, 15).unwrap())
        );
    }

    #[test]
    fn parse_after_date() {
        let q = parse_query("after:2026-01-15");
        assert_eq!(q.after, Some(NaiveDate::from_ymd_opt(2026, 1, 15).unwrap()));
    }

    #[test]
    fn parse_newer_than_days() {
        let q = parse_query("newer_than:7d");
        assert!(q.after.is_some());
        let expected = chrono::Utc::now().date_naive() - chrono::Duration::days(7);
        assert_eq!(q.after.unwrap(), expected);
    }

    #[test]
    fn parse_newer_than_weeks() {
        let q = parse_query("newer_than:2w");
        assert!(q.after.is_some());
        let expected = chrono::Utc::now().date_naive() - chrono::Duration::days(14);
        assert_eq!(q.after.unwrap(), expected);
    }

    #[test]
    fn parse_older_than_months() {
        let q = parse_query("older_than:3m");
        assert!(q.before.is_some());
        let expected = chrono::Utc::now().date_naive() - chrono::Duration::days(90);
        assert_eq!(q.before.unwrap(), expected);
    }

    #[test]
    fn parse_combined_operators_and_freetext() {
        let q = parse_query("from:@example.com has:attachment project meeting");
        assert_eq!(q.from, vec!["@example.com"]);
        assert!(q.has_attachment);
        assert_eq!(q.text, "project meeting");
    }

    #[test]
    fn parse_free_text_only() {
        let q = parse_query("hello world");
        assert_eq!(q.text, "hello world");
    }

    #[test]
    fn parse_multiple_from_values() {
        let q = parse_query("from:a@b.com from:c@d.com");
        assert_eq!(q.from, vec!["a@b.com", "c@d.com"]);
    }

    #[test]
    fn parse_newer_than_zero_ignored() {
        let q = parse_query("newer_than:0d");
        assert!(q.after.is_none());
    }

    #[test]
    fn parse_older_than_negative_ignored() {
        let q = parse_query("older_than:-5d");
        assert!(q.before.is_none());
    }

    // --- Translate tests ---

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
            after: Some(NaiveDate::from_ymd_opt(2026, 1, 15).unwrap()),
            ..Default::default()
        };
        let filter = to_jmap_filter(Some(&q), None);
        assert_eq!(filter, serde_json::json!({"after": "2026-01-15T00:00:00Z"}));
    }

    // --- Absolute date tests ---

    #[test]
    fn parse_newer_than_absolute_mm_dd_yy() {
        let q = parse_query("newer_than:01-15-25");
        assert_eq!(q.after, Some(NaiveDate::from_ymd_opt(2025, 1, 15).unwrap()));
    }

    #[test]
    fn parse_newer_than_absolute_mm_dd_yyyy() {
        let q = parse_query("newer_than:01-15-2025");
        assert_eq!(q.after, Some(NaiveDate::from_ymd_opt(2025, 1, 15).unwrap()));
    }

    #[test]
    fn parse_older_than_absolute_mm_dd_yy() {
        let q = parse_query("older_than:06-30-25");
        assert_eq!(
            q.before,
            Some(NaiveDate::from_ymd_opt(2025, 6, 30).unwrap())
        );
    }

    #[test]
    fn parse_older_than_absolute_mm_dd_yyyy() {
        let q = parse_query("older_than:06-30-2025");
        assert_eq!(
            q.before,
            Some(NaiveDate::from_ymd_opt(2025, 6, 30).unwrap())
        );
    }

    #[test]
    fn parse_newer_than_relative_still_works() {
        let q = parse_query("newer_than:7d");
        assert!(q.after.is_some());
        let expected = chrono::Utc::now().date_naive() - chrono::Duration::days(7);
        assert_eq!(q.after.unwrap(), expected);
    }

    #[test]
    fn parse_newer_than_invalid_absolute_date() {
        let q = parse_query("newer_than:13-40-25");
        assert!(q.after.is_none());
    }

    #[test]
    fn parse_newer_than_zero_weeks_ignored() {
        let q = parse_query("newer_than:0w");
        assert!(q.after.is_none());
    }

    #[test]
    fn parse_newer_than_invalid_unit() {
        let q = parse_query("newer_than:1x");
        assert!(q.after.is_none());
    }

    #[test]
    fn jmap_filter_date_before() {
        let q = ParsedQuery {
            before: Some(NaiveDate::from_ymd_opt(2026, 6, 30).unwrap()),
            ..Default::default()
        };
        let filter = to_jmap_filter(Some(&q), None);
        assert_eq!(
            filter,
            serde_json::json!({"before": "2026-06-30T00:00:00Z"})
        );
    }
}
