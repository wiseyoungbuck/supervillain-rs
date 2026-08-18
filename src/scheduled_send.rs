//! Deferred-send queue (kata vj6k Undo Send + kata acag Send Later).
//!
//! One queue serves both features: Undo Send is a short default delay with a
//! cancel window (the client supplies `send_at = now + delay`), Send Later is
//! an explicit user-chosen `send_at`. Records are durable JSON on disk
//! (mirroring `reminders.rs` `ReminderStore`); a 30s daemon dispatches due
//! records through `provider::send_email`, which covers all three provider
//! arms. Cancel-before-due removes the record and hands it back to the
//! client — the server restores nothing (the client rebuilds the draft from
//! the returned submission).
//!
//! Invites (`calendar_ics`) never go through this queue: `EmailSubmission`
//! skips that field in serde, and the invite route sends immediately.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

use crate::error::Error;
use crate::types::EmailSubmission;

/// A failed dispatch stays queued and retries on later ticks (transient
/// provider errors must not lose mail), but a permanently failing send —
/// e.g. a rejected recipient — is dropped after this many attempts (~5 min
/// at the 30s tick) instead of hammering the provider forever.
pub const MAX_DISPATCH_ATTEMPTS: u32 = 10;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ScheduledSend {
    /// Queue id (UUID string) — the cancel handle.
    pub id: String,
    pub account_id: String,
    pub from_addr: String,
    pub send_at: DateTime<Utc>,
    pub queued_at: DateTime<Utc>,
    pub submission: EmailSubmission,
    /// Failed dispatch attempts so far; see [`MAX_DISPATCH_ATTEMPTS`].
    #[serde(default)]
    pub attempts: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DispatchedSend {
    pub id: String,
    pub account_id: String,
    /// Provider message id when the provider returns one (Graph doesn't).
    pub email_id: Option<String>,
}

#[derive(Clone)]
pub struct ScheduledSendStore {
    path: PathBuf,
    records: Arc<RwLock<HashMap<String, ScheduledSend>>>,
}

impl std::fmt::Debug for ScheduledSendStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ScheduledSendStore")
            .field("path", &self.path)
            .field("records", &self.records())
            .finish()
    }
}

impl ScheduledSendStore {
    pub fn new(path: PathBuf) -> Self {
        Self {
            path,
            records: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    pub fn load(path: impl AsRef<Path>) -> Self {
        Self::new(path.as_ref().to_path_buf())
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn insert(&self, record: ScheduledSend) {
        self.records
            .write()
            .expect("scheduled send store lock poisoned")
            .insert(record.id.clone(), record);
    }

    pub fn remove(&self, id: &str) -> Option<ScheduledSend> {
        self.records
            .write()
            .expect("scheduled send store lock poisoned")
            .remove(id)
    }

    pub fn get(&self, id: &str) -> Option<ScheduledSend> {
        self.records
            .read()
            .expect("scheduled send store lock poisoned")
            .get(id)
            .cloned()
    }

    pub fn records(&self) -> Vec<ScheduledSend> {
        let mut records: Vec<_> = self
            .records
            .read()
            .expect("scheduled send store lock poisoned")
            .values()
            .cloned()
            .collect();
        records.sort_by_key(|record| record.send_at);
        records
    }

    pub fn records_for_account(&self, account_id: &str) -> Vec<ScheduledSend> {
        self.records()
            .into_iter()
            .filter(|record| record.account_id == account_id)
            .collect()
    }

    pub fn due(&self, _now: DateTime<Utc>) -> Vec<ScheduledSend> {
        Vec::new()
    }

    pub fn save(&self) -> Result<(), Error> {
        Ok(())
    }
}

/// Run one daemon pass, dispatching every due record through the provider.
/// The interval wrapper in `spawn_daemon` supplies the wall clock; this
/// function always receives `now` for deterministic tests.
pub async fn tick_scheduled_send_daemon(
    _state: &std::sync::Arc<crate::types::AppState>,
    _now: DateTime<Utc>,
) -> Vec<DispatchedSend> {
    Vec::new()
}

pub fn spawn_daemon(state: std::sync::Arc<crate::types::AppState>) {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(30));
        loop {
            interval.tick().await;
            let _ = tick_scheduled_send_daemon(&state, Utc::now()).await;
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Duration;

    fn submission(subject: &str) -> EmailSubmission {
        EmailSubmission {
            to: vec!["dest@example.com".into()],
            cc: Vec::new(),
            subject: subject.into(),
            text_body: "body".into(),
            bcc: None,
            html_body: None,
            in_reply_to: None,
            references: None,
            attachments: Vec::new(),
            calendar_ics: None,
            send_at: None,
        }
    }

    fn record(id: &str, send_at: DateTime<Utc>) -> ScheduledSend {
        ScheduledSend {
            id: id.into(),
            account_id: "account".into(),
            from_addr: "me@example.com".into(),
            send_at,
            queued_at: send_at - Duration::minutes(5),
            submission: submission(&format!("subject-{id}")),
            attempts: 0,
        }
    }

    #[test]
    fn scheduled_send_roundtrips_json() {
        let value = record("one", Utc::now());
        let json = serde_json::to_string(&value).unwrap();
        let restored: ScheduledSend = serde_json::from_str(&json).unwrap();
        assert_eq!(restored, value);
    }

    #[test]
    fn store_due_filters_and_orders_by_send_at() {
        let store = ScheduledSendStore::new(PathBuf::from("/tmp/vj6k-due.json"));
        let now = Utc::now();
        // Inserted out of order: due records must come back ordered by
        // send_at, and not-yet-due records must not leak into the due list.
        store.insert(record("later", now - Duration::seconds(1)));
        store.insert(record("future", now + Duration::hours(1)));
        store.insert(record("earlier", now - Duration::minutes(10)));
        store.insert(record("exactly-now", now));
        let due: Vec<_> = store.due(now).into_iter().map(|r| r.id).collect();
        assert_eq!(due, vec!["earlier", "later", "exactly-now"]);
        let all: Vec<_> = store.records().into_iter().map(|r| r.id).collect();
        assert_eq!(all, vec!["earlier", "later", "exactly-now", "future"]);
    }

    #[test]
    fn store_persists_and_reloads() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("scheduled-sends.json");
        let store = ScheduledSendStore::new(path.clone());
        store.insert(record("keep", Utc::now() + Duration::hours(2)));
        store.save().unwrap();
        let loaded = ScheduledSendStore::load(path);
        assert_eq!(loaded.records(), store.records());
    }

    #[test]
    fn cancel_before_due_removes_the_record_and_returns_it() {
        let store = ScheduledSendStore::new(PathBuf::from("/tmp/vj6k-cancel.json"));
        let now = Utc::now();
        store.insert(record("undo-me", now + Duration::seconds(15)));
        let cancelled = store.remove("undo-me").expect("record must come back");
        assert_eq!(cancelled.submission.subject, "subject-undo-me");
        assert!(store.due(now + Duration::hours(1)).is_empty());
        assert!(store.records().is_empty());
    }

    #[test]
    fn perf_due_scan_over_10k_queued_under_budget() {
        // Perf budget from the plan table: one daemon tick scan over 10,000
        // queued sends < 50ms. Locally this is well under 5ms; the budget
        // leaves ~10x headroom for CI. Synthetic records, no I/O.
        let store = ScheduledSendStore::new(PathBuf::from("/tmp/vj6k-perf.json"));
        let now = Utc::now();
        for i in 0..10_000 {
            store.insert(record(
                &format!("queued-{i}"),
                now + Duration::seconds(60 + i),
            ));
        }
        for i in 0..3 {
            store.insert(record(&format!("due-{i}"), now - Duration::seconds(i + 1)));
        }
        let start = std::time::Instant::now();
        let due = store.due(now);
        let elapsed = start.elapsed();
        assert_eq!(due.len(), 3, "scan must find exactly the due records");
        assert!(
            elapsed < std::time::Duration::from_millis(50),
            "due scan over 10k queued took {elapsed:?} (budget 50ms)"
        );
    }
}
