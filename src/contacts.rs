//! Contact insights (kata wcsg): server-side aggregation behind
//! `GET /api/contacts/insights` — sender history for the detail-pane
//! sidebar (message counts both directions, first/last contact, recent
//! threads). Built from two provider searches (`from:` / `to:` the
//! contact); no external data sources.

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{Email, EmailAddress};
    use chrono::{DateTime, TimeZone, Utc};
    use std::collections::HashMap;

    fn ts(day: u32, hour: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 8, day, hour, 0, 0).unwrap()
    }

    fn mk_email(
        id: &str,
        thread_id: &str,
        subject: &str,
        received_at: DateTime<Utc>,
        from_email: &str,
        from_name: Option<&str>,
    ) -> Email {
        Email {
            id: id.into(),
            blob_id: String::new(),
            thread_id: thread_id.into(),
            mailbox_ids: HashMap::new(),
            keywords: HashMap::new(),
            received_at,
            subject: subject.into(),
            from: vec![EmailAddress {
                name: from_name.map(String::from),
                email: from_email.into(),
            }],
            to: Vec::new(),
            cc: Vec::new(),
            preview: String::new(),
            has_attachment: false,
            size: 0,
            text_body: None,
            html_body: None,
            has_calendar: false,
            calendar_ics: None,
            attachments: Vec::new(),
            in_reply_to: None,
        }
    }

    const CONTACT: &str = "jane@example.com";

    #[test]
    fn aggregates_counts_and_contact_range() {
        let from_them = vec![
            mk_email("m1", "t1", "Alpha", ts(1, 9), CONTACT, None),
            mk_email("m2", "t2", "Beta", ts(3, 9), CONTACT, None),
            mk_email("m3", "t3", "Gamma", ts(5, 9), CONTACT, None),
        ];
        let to_them = vec![
            mk_email("m4", "t4", "Delta", ts(2, 9), "me@self.com", None),
            mk_email("m5", "t5", "Epsilon", ts(7, 9), "me@self.com", None),
        ];
        let insights = aggregate_insights(CONTACT, &from_them, &to_them);
        assert_eq!(insights.email, CONTACT);
        assert_eq!(insights.messages_from, 3);
        assert_eq!(insights.messages_to, 2);
        assert_eq!(insights.first_contact, Some(ts(1, 9)));
        assert_eq!(insights.last_contact, Some(ts(7, 9)));
    }

    #[test]
    fn name_comes_from_most_recent_named_message() {
        let from_them = vec![
            mk_email("m1", "t1", "Old", ts(1, 9), CONTACT, Some("Jane Old")),
            mk_email("m2", "t2", "New", ts(6, 9), CONTACT, Some("Jane Doe")),
            // Most recent of all, but carries no display name — must not
            // blank out the name.
            mk_email("m3", "t3", "Newest", ts(8, 9), CONTACT, None),
        ];
        let insights = aggregate_insights(CONTACT, &from_them, &[]);
        assert_eq!(insights.name, "Jane Doe");
    }

    #[test]
    fn recent_threads_dedup_by_thread_and_sort_desc() {
        // Two messages in t1: the thread appears once, represented by the
        // most recent message.
        let from_them = vec![
            mk_email("m1", "t1", "Re: Budget", ts(4, 9), CONTACT, None),
            mk_email("m2", "t1", "Budget", ts(1, 9), CONTACT, None),
            mk_email("m3", "t2", "Standup", ts(2, 9), CONTACT, None),
        ];
        let insights = aggregate_insights(CONTACT, &from_them, &[]);
        assert_eq!(insights.recent_threads.len(), 2);
        assert_eq!(insights.recent_threads[0].thread_id, "t1");
        assert_eq!(insights.recent_threads[0].subject, "Re: Budget");
        assert_eq!(insights.recent_threads[0].received_at, ts(4, 9));
        assert_eq!(insights.recent_threads[1].thread_id, "t2");
    }

    #[test]
    fn recent_threads_capped() {
        let from_them: Vec<Email> = (0..(RECENT_THREADS_CAP + 3))
            .map(|i| {
                mk_email(
                    &format!("m{i}"),
                    &format!("t{i}"),
                    &format!("Subject {i}"),
                    ts(1, i as u32),
                    CONTACT,
                    None,
                )
            })
            .collect();
        let insights = aggregate_insights(CONTACT, &from_them, &[]);
        assert_eq!(insights.recent_threads.len(), RECENT_THREADS_CAP);
        // Newest first: the highest timestamps survive the cap.
        assert_eq!(insights.recent_threads[0].received_at, ts(1, 7));
    }

    #[test]
    fn thread_in_both_directions_keeps_most_recent_occurrence() {
        let from_them = vec![mk_email("m1", "t1", "Re: Plans", ts(5, 9), CONTACT, None)];
        let to_them = vec![mk_email("m2", "t1", "Plans", ts(2, 9), "me@self.com", None)];
        let insights = aggregate_insights(CONTACT, &from_them, &to_them);
        assert_eq!(insights.recent_threads.len(), 1);
        assert_eq!(insights.recent_threads[0].direction, Direction::From);
        assert_eq!(insights.recent_threads[0].subject, "Re: Plans");
    }

    #[test]
    fn empty_history_yields_zeroes_not_panics() {
        let insights = aggregate_insights(CONTACT, &[], &[]);
        assert_eq!(insights.messages_from, 0);
        assert_eq!(insights.messages_to, 0);
        assert_eq!(insights.first_contact, None);
        assert_eq!(insights.last_contact, None);
        assert!(insights.recent_threads.is_empty());
        assert_eq!(insights.name, "");
    }

    #[test]
    fn threads_with_empty_thread_id_fall_back_to_message_id() {
        // JMAP properties_override paths can leave thread_id empty; two
        // distinct messages must not collapse into one "" thread.
        let from_them = vec![
            mk_email("m1", "", "One", ts(1, 9), CONTACT, None),
            mk_email("m2", "", "Two", ts(2, 9), CONTACT, None),
        ];
        let insights = aggregate_insights(CONTACT, &from_them, &[]);
        assert_eq!(insights.recent_threads.len(), 2);
    }

    #[test]
    fn wire_shape_is_camel_case() {
        let insights = aggregate_insights(
            CONTACT,
            &[mk_email("m1", "t1", "Hi", ts(1, 9), CONTACT, Some("Jane"))],
            &[],
        );
        let v = serde_json::to_value(&insights).unwrap();
        assert!(v.get("messagesFrom").is_some(), "wire: {v}");
        assert!(v.get("messagesTo").is_some());
        assert!(v.get("firstContact").is_some());
        assert!(v.get("lastContact").is_some());
        let threads = v.get("recentThreads").unwrap().as_array().unwrap();
        assert!(threads[0].get("threadId").is_some());
        assert!(threads[0].get("receivedAt").is_some());
        assert_eq!(threads[0].get("direction").unwrap(), "from");
    }

    #[test]
    fn insights_route_is_wired() {
        // Source tripwire (same style as routes.rs' own wiring pins): the
        // router chain must expose the endpoint the frontend calls.
        let routes_src = include_str!("routes.rs");
        assert!(
            routes_src.contains("\"/api/contacts/insights\""),
            "router chain must expose /api/contacts/insights"
        );
    }

    #[test]
    fn aggregation_200_messages_under_budget() {
        // Perf budget (kata wcsg): a 200-message contact (the two scan
        // limits' worth) aggregates well under 50ms. Actual cost is tens of
        // microseconds; the budget carries CI headroom (rate_limit.rs
        // discipline).
        let from_them: Vec<Email> = (0..INSIGHTS_SCAN_LIMIT)
            .map(|i| {
                mk_email(
                    &format!("f{i}"),
                    &format!("t{}", i % 40),
                    &format!("Subject {i}"),
                    ts(1 + (i % 27) as u32, (i % 24) as u32),
                    CONTACT,
                    Some("Jane Doe"),
                )
            })
            .collect();
        let to_them: Vec<Email> = (0..INSIGHTS_SCAN_LIMIT)
            .map(|i| {
                mk_email(
                    &format!("s{i}"),
                    &format!("t{}", 40 + (i % 40)),
                    &format!("Sent {i}"),
                    ts(1 + (i % 27) as u32, (i % 24) as u32),
                    "me@self.com",
                    None,
                )
            })
            .collect();
        let start = std::time::Instant::now();
        let insights = aggregate_insights(CONTACT, &from_them, &to_them);
        let elapsed = start.elapsed();
        assert_eq!(insights.messages_from, INSIGHTS_SCAN_LIMIT);
        assert_eq!(insights.recent_threads.len(), RECENT_THREADS_CAP);
        assert!(
            elapsed < std::time::Duration::from_millis(50),
            "aggregating 200 messages took {elapsed:?}, budget 50ms"
        );
    }
}
