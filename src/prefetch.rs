//! Background prefetch cache for instant account switching.
//!
//! Holds per-account snapshots of the four payloads the UI requests when a
//! user clicks an account: mailbox list, identities, default-inbox email
//! list, and split-counts for the inbox. A background warmer (see
//! `spawn_warmer`) keeps these warm so account switches return from cache
//! in <10 ms instead of waiting on ~1500 provider API calls (~24 s for
//! Gmail split-counts).

use crate::types::{Email, Identity, Mailbox};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::{Mutex, RwLock};

/// Lookup key for the cached inbox email list. Two cache hits must share
/// the same mailbox and limit; otherwise the cached payload doesn't match
/// what the caller would have fetched.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct InboxKey {
    pub mailbox_id: String,
    pub limit: usize,
}

#[derive(Default)]
struct AccountEntry {
    mailboxes: Option<Vec<Mailbox>>,
    identities: Option<Vec<Identity>>,
    inbox_list: Option<(InboxKey, Vec<Email>)>,
    split_counts: Option<(String, HashMap<String, u32>)>,
    /// Monotonic version bumped on every `invalidate`. The warmer snapshots
    /// this before each provider call and discards its result if the version
    /// changed mid-flight — otherwise a slow in-flight refresh could
    /// overwrite a freshly invalidated entry with stale data.
    version: u64,
}

pub struct PrefetchCache {
    inner: RwLock<HashMap<String, Arc<Mutex<AccountEntry>>>>,
}

impl PrefetchCache {
    pub fn new() -> Self {
        Self {
            inner: RwLock::new(HashMap::new()),
        }
    }

    async fn entry(&self, account: &str) -> Arc<Mutex<AccountEntry>> {
        {
            let r = self.inner.read().await;
            if let Some(e) = r.get(account) {
                return e.clone();
            }
        }
        let mut w = self.inner.write().await;
        w.entry(account.to_string())
            .or_insert_with(|| Arc::new(Mutex::new(AccountEntry::default())))
            .clone()
    }

    pub async fn get_mailboxes(&self, account: &str) -> Option<Vec<Mailbox>> {
        let entry = self.entry(account).await;
        entry.lock().await.mailboxes.clone()
    }

    pub async fn set_mailboxes(&self, account: &str, mailboxes: Vec<Mailbox>) {
        let entry = self.entry(account).await;
        entry.lock().await.mailboxes = Some(mailboxes);
    }

    pub async fn get_identities(&self, account: &str) -> Option<Vec<Identity>> {
        let entry = self.entry(account).await;
        entry.lock().await.identities.clone()
    }

    pub async fn set_identities(&self, account: &str, identities: Vec<Identity>) {
        let entry = self.entry(account).await;
        entry.lock().await.identities = Some(identities);
    }

    pub async fn get_inbox_list(&self, account: &str, key: &InboxKey) -> Option<Vec<Email>> {
        let entry = self.entry(account).await;
        let guard = entry.lock().await;
        match &guard.inbox_list {
            Some((k, v)) if k == key => Some(v.clone()),
            _ => None,
        }
    }

    pub async fn set_inbox_list(&self, account: &str, key: InboxKey, emails: Vec<Email>) {
        let entry = self.entry(account).await;
        entry.lock().await.inbox_list = Some((key, emails));
    }

    pub async fn get_split_counts(
        &self,
        account: &str,
        mailbox_id: &str,
    ) -> Option<HashMap<String, u32>> {
        let entry = self.entry(account).await;
        let guard = entry.lock().await;
        match &guard.split_counts {
            Some((m, c)) if m == mailbox_id => Some(c.clone()),
            _ => None,
        }
    }

    pub async fn set_split_counts(
        &self,
        account: &str,
        mailbox_id: String,
        counts: HashMap<String, u32>,
    ) {
        let entry = self.entry(account).await;
        entry.lock().await.split_counts = Some((mailbox_id, counts));
    }

    /// Clears all four cached fields and bumps the version counter. Called
    /// from mutation routes (archive / mark-read / delete / move / star) so
    /// the next read repopulates from the live provider instead of serving
    /// pre-mutation data.
    pub async fn invalidate(&self, account: &str) {
        let entry = self.entry(account).await;
        let mut e = entry.lock().await;
        e.mailboxes = None;
        e.identities = None;
        e.inbox_list = None;
        e.split_counts = None;
        e.version = e.version.wrapping_add(1);
    }

    /// Current version of this account's entry. Used by the warmer to
    /// detect mid-flight invalidations.
    pub async fn version(&self, account: &str) -> u64 {
        let entry = self.entry(account).await;
        entry.lock().await.version
    }
}

impl Default for PrefetchCache {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;

    fn mb(id: &str) -> Mailbox {
        Mailbox {
            id: id.into(),
            name: id.into(),
            role: None,
            total_emails: 0,
            unread_emails: 0,
            parent_id: None,
        }
    }

    fn ident(id: &str) -> Identity {
        Identity {
            id: id.into(),
            email: format!("{id}@example.com"),
            name: id.into(),
        }
    }

    fn email(id: &str) -> Email {
        Email {
            id: id.into(),
            blob_id: String::new(),
            thread_id: String::new(),
            mailbox_ids: HashMap::new(),
            keywords: HashMap::new(),
            received_at: Utc::now(),
            subject: String::new(),
            from: vec![],
            to: vec![],
            cc: vec![],
            preview: String::new(),
            has_attachment: false,
            size: 0,
            text_body: None,
            html_body: None,
            has_calendar: false,
            attachments: vec![],
        }
    }

    #[tokio::test]
    async fn get_mailboxes_returns_none_on_empty_cache() {
        let cache = PrefetchCache::new();
        assert!(cache.get_mailboxes("acc-1").await.is_none());
    }

    #[tokio::test]
    async fn set_then_get_mailboxes_roundtrip() {
        let cache = PrefetchCache::new();
        cache
            .set_mailboxes("acc-1", vec![mb("inbox"), mb("sent")])
            .await;
        let got = cache.get_mailboxes("acc-1").await.unwrap();
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].id, "inbox");
    }

    #[tokio::test]
    async fn set_then_get_identities_roundtrip() {
        let cache = PrefetchCache::new();
        cache.set_identities("acc-1", vec![ident("primary")]).await;
        let got = cache.get_identities("acc-1").await.unwrap();
        assert_eq!(got[0].id, "primary");
    }

    #[tokio::test]
    async fn set_then_get_inbox_list_roundtrip() {
        let cache = PrefetchCache::new();
        let key = InboxKey {
            mailbox_id: "inbox".into(),
            limit: 150,
        };
        cache
            .set_inbox_list("acc-1", key.clone(), vec![email("e1")])
            .await;
        let got = cache.get_inbox_list("acc-1", &key).await.unwrap();
        assert_eq!(got[0].id, "e1");
    }

    #[tokio::test]
    async fn account_a_does_not_leak_into_account_b() {
        let cache = PrefetchCache::new();
        cache.set_mailboxes("acc-a", vec![mb("inbox-a")]).await;
        assert!(cache.get_mailboxes("acc-b").await.is_none());
        cache.set_mailboxes("acc-b", vec![mb("inbox-b")]).await;
        let a = cache.get_mailboxes("acc-a").await.unwrap();
        let b = cache.get_mailboxes("acc-b").await.unwrap();
        assert_eq!(a[0].id, "inbox-a");
        assert_eq!(b[0].id, "inbox-b");
    }

    #[tokio::test]
    async fn account_a_lock_does_not_block_account_b_read() {
        // Holding one account's entry lock must not block reads on another
        // account; per-account `Mutex` is the contract a single global
        // Mutex would silently violate.
        let cache = Arc::new(PrefetchCache::new());
        cache.set_mailboxes("acc-a", vec![mb("a")]).await;
        cache.set_mailboxes("acc-b", vec![mb("b")]).await;

        let entry_a = cache.entry("acc-a").await;
        let _held = entry_a.lock().await;

        let cache2 = cache.clone();
        let b_read = tokio::spawn(async move { cache2.get_mailboxes("acc-b").await });

        let got = tokio::time::timeout(std::time::Duration::from_millis(100), b_read)
            .await
            .expect("account-b read should not be blocked by account-a's lock")
            .unwrap();
        assert_eq!(got.unwrap()[0].id, "b");
    }

    #[tokio::test]
    async fn inbox_list_keyed_by_mailbox_and_limit() {
        let cache = PrefetchCache::new();
        let k150 = InboxKey {
            mailbox_id: "inbox".into(),
            limit: 150,
        };
        cache
            .set_inbox_list("acc-1", k150.clone(), vec![email("e1")])
            .await;

        // Same key: hit
        assert!(cache.get_inbox_list("acc-1", &k150).await.is_some());

        // Different limit: miss
        let k50 = InboxKey {
            mailbox_id: "inbox".into(),
            limit: 50,
        };
        assert!(cache.get_inbox_list("acc-1", &k50).await.is_none());

        // Different mailbox: miss
        let k_other = InboxKey {
            mailbox_id: "archive".into(),
            limit: 150,
        };
        assert!(cache.get_inbox_list("acc-1", &k_other).await.is_none());
    }

    #[tokio::test]
    async fn invalidate_clears_all_fields_and_bumps_version() {
        let cache = PrefetchCache::new();
        let key = InboxKey {
            mailbox_id: "inbox".into(),
            limit: 150,
        };
        cache.set_mailboxes("acc-1", vec![mb("inbox")]).await;
        cache.set_identities("acc-1", vec![ident("p")]).await;
        cache
            .set_inbox_list("acc-1", key.clone(), vec![email("e1")])
            .await;
        cache
            .set_split_counts("acc-1", "inbox".into(), HashMap::new())
            .await;

        let v0 = cache.version("acc-1").await;
        cache.invalidate("acc-1").await;

        assert!(cache.get_mailboxes("acc-1").await.is_none());
        assert!(cache.get_identities("acc-1").await.is_none());
        assert!(cache.get_inbox_list("acc-1", &key).await.is_none());
        assert!(cache.get_split_counts("acc-1", "inbox").await.is_none());
        assert_eq!(cache.version("acc-1").await, v0 + 1);
    }

    #[tokio::test]
    async fn set_then_get_split_counts_roundtrip() {
        let cache = PrefetchCache::new();
        let mut counts = HashMap::new();
        counts.insert("split-a".into(), 7);
        cache
            .set_split_counts("acc-1", "inbox".into(), counts.clone())
            .await;
        let got = cache.get_split_counts("acc-1", "inbox").await.unwrap();
        assert_eq!(got.get("split-a"), Some(&7));
    }
}
