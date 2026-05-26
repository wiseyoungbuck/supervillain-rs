//! Background prefetch cache for instant account switching.
//!
//! Holds per-account snapshots of the four payloads the UI requests when a
//! user clicks an account: mailbox list, identities, default-inbox email
//! list, and split-counts for the inbox. A background warmer (see
//! `spawn_warmer`) keeps these warm so account switches return from cache
//! in <10 ms instead of waiting on ~1500 provider API calls (~24 s for
//! Gmail split-counts).

use crate::error::Error;
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

    // ---- Version-guarded setters used by the background warmer ----
    //
    // Each `try_set_*` is an atomic check-and-set against the version
    // counter: returns true if the version still matched and the value
    // was written, false if the version had changed (a mutation ran
    // mid-fetch) and the result was discarded. This is what closes the
    // race window between "warmer started a slow fetch" and "user
    // archived an email": without the check, the warmer's stale result
    // would overwrite the invalidated entry seconds after the user's
    // action.

    pub async fn try_set_mailboxes(
        &self,
        account: &str,
        expected_version: u64,
        mailboxes: Vec<Mailbox>,
    ) -> bool {
        let entry = self.entry(account).await;
        let mut e = entry.lock().await;
        if e.version != expected_version {
            return false;
        }
        e.mailboxes = Some(mailboxes);
        true
    }

    pub async fn try_set_identities(
        &self,
        account: &str,
        expected_version: u64,
        identities: Vec<Identity>,
    ) -> bool {
        let entry = self.entry(account).await;
        let mut e = entry.lock().await;
        if e.version != expected_version {
            return false;
        }
        e.identities = Some(identities);
        true
    }

    pub async fn try_set_inbox_list(
        &self,
        account: &str,
        expected_version: u64,
        key: InboxKey,
        emails: Vec<Email>,
    ) -> bool {
        let entry = self.entry(account).await;
        let mut e = entry.lock().await;
        if e.version != expected_version {
            return false;
        }
        e.inbox_list = Some((key, emails));
        true
    }

    pub async fn try_set_split_counts(
        &self,
        account: &str,
        expected_version: u64,
        mailbox_id: String,
        counts: HashMap<String, u32>,
    ) -> bool {
        let entry = self.entry(account).await;
        let mut e = entry.lock().await;
        if e.version != expected_version {
            return false;
        }
        e.split_counts = Some((mailbox_id, counts));
        true
    }

    // ---- "or-fetch" helpers used by the route handlers ----
    //
    // Each one is the canonical cache-aware accessor: returns cached data
    // if present, otherwise calls the fetch closure, populates the cache,
    // and returns the live data. Routes use these instead of poking the
    // cache directly so the hit/miss contract is enforced in one place.

    pub async fn mailboxes_or_fetch<F, Fut>(
        &self,
        account: &str,
        fetch: F,
    ) -> Result<Vec<Mailbox>, Error>
    where
        F: FnOnce() -> Fut,
        Fut: std::future::Future<Output = Result<Vec<Mailbox>, Error>>,
    {
        if let Some(c) = self.get_mailboxes(account).await {
            return Ok(c);
        }
        let live = fetch().await?;
        self.set_mailboxes(account, live.clone()).await;
        Ok(live)
    }

    pub async fn identities_or_fetch<F, Fut>(
        &self,
        account: &str,
        fetch: F,
    ) -> Result<Vec<Identity>, Error>
    where
        F: FnOnce() -> Fut,
        Fut: std::future::Future<Output = Result<Vec<Identity>, Error>>,
    {
        if let Some(c) = self.get_identities(account).await {
            return Ok(c);
        }
        let live = fetch().await?;
        self.set_identities(account, live.clone()).await;
        Ok(live)
    }

    pub async fn inbox_list_or_fetch<F, Fut>(
        &self,
        account: &str,
        key: InboxKey,
        fetch: F,
    ) -> Result<Vec<Email>, Error>
    where
        F: FnOnce() -> Fut,
        Fut: std::future::Future<Output = Result<Vec<Email>, Error>>,
    {
        if let Some(c) = self.get_inbox_list(account, &key).await {
            return Ok(c);
        }
        let live = fetch().await?;
        self.set_inbox_list(account, key, live.clone()).await;
        Ok(live)
    }

    pub async fn split_counts_or_fetch<F, Fut>(
        &self,
        account: &str,
        mailbox_id: &str,
        fetch: F,
    ) -> Result<HashMap<String, u32>, Error>
    where
        F: FnOnce() -> Fut,
        Fut: std::future::Future<Output = Result<HashMap<String, u32>, Error>>,
    {
        if let Some(c) = self.get_split_counts(account, mailbox_id).await {
            return Ok(c);
        }
        let live = fetch().await?;
        self.set_split_counts(account, mailbox_id.to_string(), live.clone())
            .await;
        Ok(live)
    }
}

impl Default for PrefetchCache {
    fn default() -> Self {
        Self::new()
    }
}

// =============================================================================
// Background warmer
// =============================================================================

/// Warm one account's cache: mailboxes, identities, inbox-list, split-counts.
///
/// Each step that fails is logged but does not abort the others — a transient
/// network blip on identities shouldn't poison the mailbox cache. We snapshot
/// the entry version before each fetch and discard the write if the version
/// changed mid-flight, so a user-triggered mutation that runs in parallel
/// always wins (see [`PrefetchCache::invalidate`]).
pub async fn prefetch_account(state: &crate::types::AppState, account_id: &str) {
    let started = std::time::Instant::now();
    let cache = state.prefetch.clone();

    // Snapshot the version once and reuse it for every try_set; if any
    // invalidation fires during this pass, every subsequent write is
    // discarded atomically. The cost of a missed warm pass is just one
    // extra live fetch on the next user request.
    let v = cache.version(account_id).await;

    // --- mailboxes ---
    let mailboxes = match fetch_mailboxes(state, account_id).await {
        Ok(m) => {
            if cache.try_set_mailboxes(account_id, v, m.clone()).await {
                Some(m)
            } else {
                tracing::debug!(
                    account = %account_id,
                    "prefetch: mailboxes discarded — version changed mid-fetch"
                );
                Some(m) // still return mailboxes so we can find the inbox below
            }
        }
        Err(e) => {
            tracing::warn!(account = %account_id, "prefetch: mailboxes failed: {e}");
            None
        }
    };

    // --- identities ---
    match fetch_identities(state, account_id).await {
        Ok(ids) => {
            let _ = cache.try_set_identities(account_id, v, ids).await;
        }
        Err(e) => tracing::warn!(account = %account_id, "prefetch: identities failed: {e}"),
    }

    // --- inbox + split counts (only if we have mailboxes to find the inbox role) ---
    if let Some(mailboxes) = mailboxes
        && let Some(inbox) = mailboxes
            .iter()
            .find(|m| m.role.as_deref() == Some("inbox"))
    {
        let inbox_id = inbox.id.clone();
        match fetch_inbox(state, account_id, &inbox_id).await {
            Ok(emails) => {
                let _ = cache
                    .try_set_inbox_list(
                        account_id,
                        v,
                        InboxKey {
                            mailbox_id: inbox_id.clone(),
                            limit: 150,
                        },
                        emails,
                    )
                    .await;
            }
            Err(e) => tracing::warn!(account = %account_id, "prefetch: inbox list failed: {e}"),
        }

        match fetch_split_counts(state, account_id, &inbox_id).await {
            Ok(counts) => {
                let _ = cache
                    .try_set_split_counts(account_id, v, inbox_id, counts)
                    .await;
            }
            Err(e) => {
                tracing::warn!(account = %account_id, "prefetch: split-counts failed: {e}")
            }
        }
    }

    tracing::info!(
        account = %account_id,
        elapsed_ms = started.elapsed().as_millis() as u64,
        "prefetch: account warmed"
    );
}

async fn fetch_mailboxes(
    state: &crate::types::AppState,
    account_id: &str,
) -> Result<Vec<Mailbox>, Error> {
    let session_lock = session_for(state, account_id).await?;
    let session = session_lock.read().await;
    crate::provider::get_mailboxes(&session).await
}

async fn fetch_identities(
    state: &crate::types::AppState,
    account_id: &str,
) -> Result<Vec<Identity>, Error> {
    let session_lock = session_for(state, account_id).await?;
    let mut session = session_lock.write().await;
    crate::provider::get_identities(&mut session).await
}

async fn fetch_inbox(
    state: &crate::types::AppState,
    account_id: &str,
    mailbox_id: &str,
) -> Result<Vec<Email>, Error> {
    let session_lock = session_for(state, account_id).await?;
    let session = session_lock.read().await;
    let ids = crate::provider::query_emails(&session, Some(mailbox_id), 150, 0, None).await?;
    crate::provider::get_emails(&session, &ids, false, None).await
}

async fn fetch_split_counts(
    state: &crate::types::AppState,
    account_id: &str,
    mailbox_id: &str,
) -> Result<HashMap<String, u32>, Error> {
    let config = crate::splits::load_splits(
        &state.splits_config_path,
        std::env::var("VIMMAIL_SPLITS").ok().as_deref(),
    );
    if config.splits.is_empty() {
        return Ok(HashMap::new());
    }
    let session_lock = session_for(state, account_id).await?;
    let session = session_lock.read().await;
    let fetch_limit = 150 * 10;
    let ids =
        crate::provider::query_emails(&session, Some(mailbox_id), fetch_limit, 0, None).await?;
    let minimal_props: &[&str] = &["id", "from", "to", "cc", "subject"];
    let mut all = Vec::new();
    for batch in ids.chunks(500) {
        let part = crate::provider::get_emails(&session, batch, false, Some(minimal_props)).await?;
        all.extend(part);
    }
    let mut counts = HashMap::new();
    for split in &config.splits {
        let n = all
            .iter()
            .filter(|e| crate::splits::matches_split(e, split))
            .count();
        counts.insert(split.id.clone(), n as u32);
    }
    Ok(counts)
}

async fn session_for(
    state: &crate::types::AppState,
    account_id: &str,
) -> Result<crate::types::SessionLock, Error> {
    let reg = state.accounts.read().await;
    reg.sessions
        .get(account_id)
        .cloned()
        .ok_or_else(|| Error::BadRequest(format!("Unknown account: {account_id}")))
}

/// Generic warm-and-refresh loop, parameterised over how to list accounts
/// and how to warm one. The production wrapper is [`spawn_warmer`]; this
/// inner form exists so the interval + iteration behaviour can be tested
/// without building a real `AppState` or hitting any provider.
///
/// The 200 ms startup delay lets the HTTP server bind before the warmer
/// competes for the rate limiter — measured in real time on production,
/// but `tokio::time::pause` lets tests step over it instantly.
pub async fn warm_loop<L, LFut, W, WFut>(
    list_accounts: L,
    interval: std::time::Duration,
    warm_account: W,
) where
    L: Fn() -> LFut,
    LFut: std::future::Future<Output = Vec<String>>,
    W: Fn(String) -> WFut,
    WFut: std::future::Future<Output = ()>,
{
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    loop {
        let accounts = list_accounts().await;
        let n = accounts.len();
        tracing::info!(count = n, "prefetch: warming pass starting");
        for account in accounts {
            warm_account(account).await;
        }
        tracing::info!(count = n, "prefetch: warming pass complete");
        tokio::time::sleep(interval).await;
    }
}

/// Spawn the background warm-and-refresh loop. Runs an initial warm pass
/// for every configured account, then sleeps `interval` between passes.
pub fn spawn_warmer(
    state: std::sync::Arc<crate::types::AppState>,
    interval: std::time::Duration,
) -> tokio::task::JoinHandle<()> {
    let state_for_list = state.clone();
    let state_for_warm = state.clone();
    tokio::spawn(async move {
        warm_loop(
            move || {
                let s = state_for_list.clone();
                async move {
                    let reg = s.accounts.read().await;
                    reg.sessions.keys().cloned().collect::<Vec<_>>()
                }
            },
            interval,
            move |account| {
                let s = state_for_warm.clone();
                async move {
                    prefetch_account(&s, &account).await;
                }
            },
        )
        .await
    })
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
    async fn mailboxes_or_fetch_returns_cached_without_calling_fetch() {
        let cache = PrefetchCache::new();
        cache.set_mailboxes("acc-1", vec![mb("from-cache")]).await;

        let got = cache
            .mailboxes_or_fetch("acc-1", || async {
                panic!("fetch closure must not be called on cache hit");
            })
            .await
            .unwrap();
        assert_eq!(got[0].id, "from-cache");
    }

    #[tokio::test]
    async fn mailboxes_or_fetch_populates_cache_on_miss() {
        use std::sync::atomic::{AtomicU32, Ordering};
        let cache = PrefetchCache::new();
        let calls = Arc::new(AtomicU32::new(0));

        let c2 = calls.clone();
        let got = cache
            .mailboxes_or_fetch("acc-1", move || {
                let c2 = c2.clone();
                async move {
                    c2.fetch_add(1, Ordering::SeqCst);
                    Ok(vec![mb("from-live")])
                }
            })
            .await
            .unwrap();
        assert_eq!(got[0].id, "from-live");
        assert_eq!(calls.load(Ordering::SeqCst), 1);

        // Second call must hit cache — fetch closure not invoked.
        let c3 = calls.clone();
        let _ = cache
            .mailboxes_or_fetch("acc-1", move || {
                let c3 = c3.clone();
                async move {
                    c3.fetch_add(1, Ordering::SeqCst);
                    Ok(vec![])
                }
            })
            .await
            .unwrap();
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "second call must hit cache, not refetch"
        );
    }

    #[tokio::test]
    async fn identities_or_fetch_hit_and_miss() {
        use std::sync::atomic::{AtomicU32, Ordering};
        let cache = PrefetchCache::new();
        let calls = Arc::new(AtomicU32::new(0));
        let c2 = calls.clone();
        let _ = cache
            .identities_or_fetch("acc-1", move || {
                let c2 = c2.clone();
                async move {
                    c2.fetch_add(1, Ordering::SeqCst);
                    Ok(vec![ident("p")])
                }
            })
            .await
            .unwrap();
        let got = cache
            .identities_or_fetch("acc-1", || async { panic!("hit path must not refetch") })
            .await
            .unwrap();
        assert_eq!(got[0].id, "p");
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn inbox_list_or_fetch_keyed_miss_and_hit() {
        use std::sync::atomic::{AtomicU32, Ordering};
        let cache = PrefetchCache::new();
        let key = InboxKey {
            mailbox_id: "inbox".into(),
            limit: 150,
        };
        let calls = Arc::new(AtomicU32::new(0));
        let c2 = calls.clone();
        let _ = cache
            .inbox_list_or_fetch("acc-1", key.clone(), move || {
                let c2 = c2.clone();
                async move {
                    c2.fetch_add(1, Ordering::SeqCst);
                    Ok(vec![email("e1")])
                }
            })
            .await
            .unwrap();
        // Same key: cache hit
        let _ = cache
            .inbox_list_or_fetch("acc-1", key.clone(), || async {
                panic!("hit path must not refetch")
            })
            .await
            .unwrap();
        // Different limit: cache miss → refetches
        let c3 = calls.clone();
        let k2 = InboxKey {
            mailbox_id: "inbox".into(),
            limit: 50,
        };
        let _ = cache
            .inbox_list_or_fetch("acc-1", k2, move || {
                let c3 = c3.clone();
                async move {
                    c3.fetch_add(1, Ordering::SeqCst);
                    Ok(vec![])
                }
            })
            .await
            .unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn split_counts_or_fetch_hit_and_miss() {
        use std::sync::atomic::{AtomicU32, Ordering};
        let cache = PrefetchCache::new();
        let calls = Arc::new(AtomicU32::new(0));
        let c2 = calls.clone();
        let _ = cache
            .split_counts_or_fetch("acc-1", "inbox", move || {
                let c2 = c2.clone();
                async move {
                    c2.fetch_add(1, Ordering::SeqCst);
                    let mut m = HashMap::new();
                    m.insert("split-a".into(), 3);
                    Ok(m)
                }
            })
            .await
            .unwrap();
        let got = cache
            .split_counts_or_fetch("acc-1", "inbox", || async {
                panic!("hit path must not refetch")
            })
            .await
            .unwrap();
        assert_eq!(got.get("split-a"), Some(&3));
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn try_set_mailboxes_skips_when_version_changed() {
        let cache = PrefetchCache::new();
        let v0 = cache.version("acc-1").await;
        cache.invalidate("acc-1").await; // bumps version
        let set = cache
            .try_set_mailboxes("acc-1", v0, vec![mb("stale")])
            .await;
        assert!(!set, "stale write must be discarded");
        assert!(
            cache.get_mailboxes("acc-1").await.is_none(),
            "cache must remain cleared"
        );
    }

    #[tokio::test]
    async fn try_set_mailboxes_succeeds_when_version_matches() {
        let cache = PrefetchCache::new();
        let v0 = cache.version("acc-1").await;
        let set = cache
            .try_set_mailboxes("acc-1", v0, vec![mb("fresh")])
            .await;
        assert!(set);
        assert_eq!(cache.get_mailboxes("acc-1").await.unwrap()[0].id, "fresh");
    }

    #[tokio::test]
    async fn try_set_inbox_list_skips_when_version_changed() {
        let cache = PrefetchCache::new();
        let v0 = cache.version("acc-1").await;
        cache.invalidate("acc-1").await;
        let key = InboxKey {
            mailbox_id: "inbox".into(),
            limit: 150,
        };
        let set = cache
            .try_set_inbox_list("acc-1", v0, key.clone(), vec![email("stale")])
            .await;
        assert!(!set);
        assert!(cache.get_inbox_list("acc-1", &key).await.is_none());
    }

    #[tokio::test(start_paused = true)]
    async fn warm_loop_runs_initial_pass_then_repeats_each_interval() {
        use std::sync::atomic::{AtomicU32, Ordering};
        let calls = Arc::new(AtomicU32::new(0));
        let interval = std::time::Duration::from_secs(300);

        let cls = calls.clone();
        let handle = tokio::spawn(async move {
            warm_loop(
                || async { vec!["acc-1".to_string()] },
                interval,
                move |_acc| {
                    let c = cls.clone();
                    async move {
                        c.fetch_add(1, Ordering::SeqCst);
                    }
                },
            )
            .await;
        });

        // Move past startup delay + the initial pass.
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "initial pass should have completed"
        );

        // Skip one interval — the second pass should run.
        tokio::time::sleep(interval).await;
        // The next sleep call inside the loop yields; advance past it.
        tokio::time::sleep(std::time::Duration::from_millis(1)).await;
        assert_eq!(
            calls.load(Ordering::SeqCst),
            2,
            "second pass should run after one interval"
        );

        handle.abort();
    }

    #[tokio::test(start_paused = true)]
    async fn warm_loop_warms_every_account_each_pass_even_when_one_logs_an_error() {
        // `prefetch_account` absorbs all errors into tracing and returns
        // (); the contract this test pins is that the loop calls the
        // closure for *every* account, even if a previous one took the
        // error path internally.
        let seen = Arc::new(Mutex::new(Vec::new()));
        let s2 = seen.clone();
        let handle = tokio::spawn(async move {
            warm_loop(
                || async { vec!["good".to_string(), "bad".to_string(), "good2".to_string()] },
                std::time::Duration::from_secs(300),
                move |a| {
                    let s = s2.clone();
                    async move {
                        // "bad" simulates the per-account error path: do
                        // some no-op work and return without panicking.
                        if a == "bad" {
                            tracing::debug!("simulated per-account warm error");
                        }
                        s.lock().await.push(a);
                    }
                },
            )
            .await;
        });

        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
        let got = seen.lock().await.clone();
        assert_eq!(
            got,
            vec!["good".to_string(), "bad".to_string(), "good2".to_string()],
            "every account must be visited each pass"
        );
        handle.abort();
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
