//! Contact insights (kata wcsg): server-side aggregation behind
//! `GET /api/contacts/insights` — sender history for the detail-pane
//! sidebar (message counts both directions, first/last contact, recent
//! threads). Built from two provider searches (`from:` / `to:` the
//! contact); no external data sources.

use crate::error::Error;
use crate::provider::{self, ProviderSession};
use crate::types::{AppState, Email, EmailSort, ParsedQuery};
use axum::extract::{Query, State};
use axum::response::Json;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

/// Threads shown in the sidebar. Server-capped so the payload stays flat
/// no matter how deep the history runs.
pub const RECENT_THREADS_CAP: usize = 5;

/// Per-direction provider search window. 100 each way = a "200-message
/// contact" is fully aggregated in one request; deeper history saturates
/// the counts at the cap, which the sidebar presents as frequency, not an
/// exact ledger.
pub const INSIGHTS_SCAN_LIMIT: usize = 100;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Direction {
    /// The contact sent it to us.
    From,
    /// We sent it to the contact.
    To,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ThreadStub {
    pub thread_id: String,
    pub email_id: String,
    pub subject: String,
    pub received_at: DateTime<Utc>,
    pub direction: Direction,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ContactInsights {
    pub email: String,
    /// Display name from the most recent message that carried one; empty
    /// when the contact never sent a named address.
    pub name: String,
    pub messages_from: usize,
    pub messages_to: usize,
    pub first_contact: Option<DateTime<Utc>>,
    pub last_contact: Option<DateTime<Utc>>,
    /// Newest-first, deduped by thread, capped at [`RECENT_THREADS_CAP`].
    pub recent_threads: Vec<ThreadStub>,
}

/// Pure aggregation over the two search results. `from_them` are messages
/// the contact sent; `to_them` are messages we sent to the contact.
pub fn aggregate_insights(
    contact_email: &str,
    from_them: &[Email],
    to_them: &[Email],
) -> ContactInsights {
    let mut first_contact: Option<DateTime<Utc>> = None;
    let mut last_contact: Option<DateTime<Utc>> = None;
    for email in from_them.iter().chain(to_them) {
        if first_contact.is_none_or(|t| email.received_at < t) {
            first_contact = Some(email.received_at);
        }
        if last_contact.is_none_or(|t| email.received_at > t) {
            last_contact = Some(email.received_at);
        }
    }

    // Display name: the most recent message where the contact's address
    // carried one. A newer message without a name must not blank it out.
    let mut name = String::new();
    let mut name_ts: Option<DateTime<Utc>> = None;
    for email in from_them {
        let named = email
            .from
            .iter()
            .find(|a| a.email.eq_ignore_ascii_case(contact_email))
            .and_then(|a| a.name.as_deref())
            .filter(|n| !n.is_empty());
        if let Some(n) = named
            && name_ts.is_none_or(|t| email.received_at > t)
        {
            name = n.to_string();
            name_ts = Some(email.received_at);
        }
    }

    // Dedup by thread (message id when the provider left thread_id empty),
    // keeping each thread's most recent message — direction and subject
    // travel with it.
    let mut threads: std::collections::HashMap<&str, ThreadStub> = std::collections::HashMap::new();
    let tagged = from_them
        .iter()
        .map(|e| (e, Direction::From))
        .chain(to_them.iter().map(|e| (e, Direction::To)));
    for (email, direction) in tagged {
        let key: &str = if email.thread_id.is_empty() {
            &email.id
        } else {
            &email.thread_id
        };
        let newer = threads
            .get(key)
            .is_none_or(|existing| email.received_at > existing.received_at);
        if newer {
            threads.insert(
                key,
                ThreadStub {
                    thread_id: email.thread_id.clone(),
                    email_id: email.id.clone(),
                    subject: email.subject.clone(),
                    received_at: email.received_at,
                    direction,
                },
            );
        }
    }
    let mut recent_threads: Vec<ThreadStub> = threads.into_values().collect();
    recent_threads.sort_by(|a, b| b.received_at.cmp(&a.received_at));
    recent_threads.truncate(RECENT_THREADS_CAP);

    ContactInsights {
        email: contact_email.to_string(),
        name,
        messages_from: from_them.len(),
        messages_to: to_them.len(),
        first_contact,
        last_contact,
        recent_threads,
    }
}

#[derive(Deserialize)]
pub struct InsightsParams {
    pub email: String,
    pub account: Option<String>,
}

/// `GET /api/contacts/insights?email=<addr>` — aggregate this account's
/// history with one contact. Two provider searches (from:/to:, all
/// mailboxes) run concurrently, then one metadata fetch per direction;
/// bodies are never fetched.
pub async fn contact_insights(
    State(state): State<Arc<AppState>>,
    Query(params): Query<InsightsParams>,
) -> Result<Json<ContactInsights>, Error> {
    let contact = params.email.trim().to_ascii_lowercase();
    if crate::accounts::validate_email(&contact).is_err() {
        return Err(Error::BadRequest(format!(
            "invalid contact email '{}'",
            params.email
        )));
    }

    let session_lock = crate::routes::resolve_session(&state, params.account.as_deref()).await?;
    let session = session_lock.read().await;

    let from_query = ParsedQuery {
        from: vec![contact.clone()],
        ..Default::default()
    };
    let to_query = ParsedQuery {
        to: vec![contact.clone()],
        ..Default::default()
    };
    let (from_ids, to_ids) = tokio::join!(
        provider::query_emails(
            &session,
            None,
            INSIGHTS_SCAN_LIMIT,
            0,
            Some(&from_query),
            EmailSort::default(),
        ),
        provider::query_emails(
            &session,
            None,
            INSIGHTS_SCAN_LIMIT,
            0,
            Some(&to_query),
            EmailSort::default(),
        ),
    );
    let (from_ids, to_ids) = (from_ids?, to_ids?);

    let (from_them, to_them) = tokio::join!(
        fetch_metadata(&session, &from_ids),
        fetch_metadata(&session, &to_ids),
    );
    let insights = aggregate_insights(&contact, &from_them?, &to_them?);
    Ok(Json(insights))
}

async fn fetch_metadata(session: &ProviderSession, ids: &[String]) -> Result<Vec<Email>, Error> {
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    provider::get_emails(session, ids, false, None, false).await
}

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
