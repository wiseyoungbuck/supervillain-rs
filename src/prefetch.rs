//! Background prefetch cache for instant account switching.
//!
//! Holds per-account snapshots of the four payloads the UI requests when a
//! user clicks an account: mailbox list, identities, default-inbox email
//! list, and split-counts for the inbox. A background warmer (see
//! `spawn_warmer`) keeps these warm so account switches return from cache
//! in <10 ms instead of waiting on ~1500 provider API calls (~24 s for
//! Gmail split-counts).

use crate::error::Error;
use crate::types::{Email, EmailSort, Identity, Mailbox};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::{Mutex, RwLock};

/// Lookup key for the cached inbox email list. Two cache hits must share
/// the same mailbox, limit, and sort order; otherwise the cached payload
/// doesn't match what the caller would have fetched.
///
/// `sort` joined this key in kata 09ef, alongside `mailbox_id`/`limit`:
/// without it, a `DateAsc` request and the warmer's always-`DateDesc`
/// entry would collide on the same slot, so whichever wrote last would
/// silently serve the wrong order to the other. The background warmer
/// only ever populates the `DateDesc` slot (see `fetch_inbox` /
/// `warm_all_mailboxes`).
///
/// As of roborev 291, `list_emails`'s `is_cacheable` gate
/// (`routes::list_is_cacheable`) additionally requires
/// `sort == EmailSort::default()` before touching this cache at all — a
/// non-default sort request never reads *or writes* a slot here anymore.
/// Letting `DateAsc` populate its own slot (the pre-291 behavior) meant
/// that slot was never refreshed by the warmer and had no TTL, so a user
/// sitting in "Oldest first" would never see new mail short of an
/// unrelated mutation invalidating the whole account. Always fetching
/// non-default sorts live is the simpler fix. This struct still carries
/// `sort` so the cache remains correct by construction if that gate is
/// ever loosened to warm additional sort orders.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct InboxKey {
    pub mailbox_id: String,
    pub limit: usize,
    pub sort: EmailSort,
}

/// One cached mailbox list plus its freshness marker. `stale` is true only
/// for entries restored from the on-disk snapshot at startup: they paint
/// instantly but reflect the previous run's state. Every live write (warmer
/// or route miss) stores `stale: false`. `list_emails` surfaces the flag via
/// the `x-supervillain-stale` response header so the frontend can keep
/// re-polling (cheap cache reads) until the warmer replaces the entry.
#[derive(Clone)]
struct InboxList {
    emails: Vec<Email>,
    stale: bool,
}

/// Per-account cache. `inbox_lists` is a map (one entry per (mailbox, limit))
/// so the warmer can warm Inbox / Archive / Sent / labels in parallel without
/// evicting each other. `body_cache` is a flat-by-id store for individual
/// email bodies — fed both by the warmer's top-N prefetch and by `get_email`
/// route hits, keyed by provider message id (unique within an account, so no
/// account-id prefix needed).
#[derive(Default)]
struct AccountEntry {
    mailboxes: Option<Vec<Mailbox>>,
    identities: Option<Vec<Identity>>,
    inbox_lists: HashMap<InboxKey, InboxList>,
    split_counts: Option<(String, HashMap<String, u32>)>,
    body_cache: HashMap<String, Email>,
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
        guard.inbox_lists.get(key).map(|l| l.emails.clone())
    }

    pub async fn set_inbox_list(&self, account: &str, key: InboxKey, emails: Vec<Email>) {
        let entry = self.entry(account).await;
        entry.lock().await.inbox_lists.insert(
            key,
            InboxList {
                emails,
                stale: false,
            },
        );
    }

    pub async fn get_body(&self, account: &str, email_id: &str) -> Option<Email> {
        let entry = self.entry(account).await;
        entry.lock().await.body_cache.get(email_id).cloned()
    }

    pub async fn set_body(&self, account: &str, email_id: String, email: Email) {
        let entry = self.entry(account).await;
        entry.lock().await.body_cache.insert(email_id, email);
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
    ///
    /// **Inbound-change staleness window:** the cache is *only* invalidated
    /// on user-initiated mutations. Mail arriving server-side (or flag
    /// changes from another client) doesn't trigger an invalidate, so it
    /// stays invisible until either the user themselves mutates something
    /// or the 5-minute warmer cycle re-fetches. If/when a push channel
    /// (JMAP push, Graph webhooks, IMAP IDLE) is wired up, that handler
    /// MUST call `invalidate(account_id)` to close this window.
    pub async fn invalidate(&self, account: &str) {
        let entry = self.entry(account).await;
        let mut e = entry.lock().await;
        e.mailboxes = None;
        e.identities = None;
        e.inbox_lists.clear();
        e.split_counts = None;
        // body_cache deliberately survives: per-mutation invalidates fire
        // on every mark-read / archive / flag-toggle, but the email's
        // text/html content doesn't change with those operations. The
        // frontend's emailCache (a50f1f8) carries the optimistically-
        // updated metadata for any email the user has touched, so stale
        // keywords in body_cache don't reach the UI. Wholesale-wiping
        // bodies on every read action would turn the cache into a one-
        // shot buffer that the next mutation always drains.
        e.version = e.version.wrapping_add(1);
    }

    /// Wholesale-clear, including body_cache. Use only for "the account
    /// was removed / tokens were revoked" type events, where keeping any
    /// previous content would be a leak rather than a freshness issue.
    pub async fn invalidate_full(&self, account: &str) {
        let entry = self.entry(account).await;
        let mut e = entry.lock().await;
        e.mailboxes = None;
        e.identities = None;
        e.inbox_lists.clear();
        e.split_counts = None;
        e.body_cache.clear();
        e.version = e.version.wrapping_add(1);
    }

    /// Clears only the cached split-counts, leaving mailboxes / identities /
    /// inbox_lists / body_cache untouched. Splits CRUD writes touch exactly
    /// this one derived field; a full `invalidate` would also discard
    /// perfectly fresh mailbox/inbox data for no reason. Still bumps the
    /// version counter so an in-flight warmer fetch computed against the
    /// pre-write splits.json is discarded instead of overwriting the fresh
    /// state with stale counts.
    pub async fn invalidate_split_counts(&self, account: &str) {
        let entry = self.entry(account).await;
        let mut e = entry.lock().await;
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
        e.inbox_lists.insert(
            key,
            InboxList {
                emails,
                stale: false,
            },
        );
        true
    }

    pub async fn try_set_body(
        &self,
        account: &str,
        expected_version: u64,
        email_id: String,
        email: Email,
    ) -> bool {
        let entry = self.entry(account).await;
        let mut e = entry.lock().await;
        if e.version != expected_version {
            return false;
        }
        e.body_cache.insert(email_id, email);
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

    /// Returns `(emails, stale)`. A stale hit (disk-restored snapshot) is
    /// still a hit — the caller gets the old list instantly instead of
    /// waiting ~12 s on a live Gmail crawl — but the flag lets the route
    /// tell the frontend to re-poll until the warmer lands fresh data.
    pub async fn inbox_list_or_fetch<F, Fut>(
        &self,
        account: &str,
        key: InboxKey,
        fetch: F,
    ) -> Result<(Vec<Email>, bool), Error>
    where
        F: FnOnce() -> Fut,
        Fut: std::future::Future<Output = Result<Vec<Email>, Error>>,
    {
        {
            let entry = self.entry(account).await;
            let guard = entry.lock().await;
            if let Some(l) = guard.inbox_lists.get(&key) {
                return Ok((l.emails.clone(), l.stale));
            }
        }
        let live = fetch().await?;
        self.set_inbox_list(account, key, live.clone()).await;
        Ok((live, false))
    }

    pub async fn body_or_fetch<F, Fut>(
        &self,
        account: &str,
        email_id: &str,
        fetch: F,
    ) -> Result<Email, Error>
    where
        F: FnOnce() -> Fut,
        Fut: std::future::Future<Output = Result<Email, Error>>,
    {
        if let Some(c) = self.get_body(account, email_id).await {
            return Ok(c);
        }
        let live = fetch().await?;
        self.set_body(account, email_id.to_string(), live.clone())
            .await;
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
// Disk persistence
// =============================================================================
//
// The cache is memory-only at runtime; these snapshots exist so a server
// restart doesn't cold-start the UI. The launcher starts the binary on
// demand, so "first open of the day" used to mean: empty cache, browser
// racing the warmer, and a ~12 s live Gmail crawl before first paint.
// Restoring the previous run's snapshot (with every inbox list marked
// stale) makes first paint instant; the warmer then replaces the stale
// entries within its first pass.

/// Newest bodies kept per account in the snapshot, ranked by `received_at`.
/// `body_cache` has no eviction in memory (bounded per warm cycle by
/// `BODY_PREFETCH_PER_MAILBOX`), but persisting it verbatim would compound
/// across restarts; the cap keeps the file to roughly what one warm cycle
/// produces anyway.
const SNAPSHOT_BODY_CAP: usize = 500;

/// Serialized form of one account's cache entry. `inbox_lists` is a Vec of
/// pairs (not a map) because `InboxKey` is a struct and JSON object keys
/// must be strings. The `stale` flags are deliberately NOT persisted —
/// everything in a snapshot is stale by definition when it's read back.
#[derive(Serialize, Deserialize)]
struct AccountSnapshot {
    mailboxes: Option<Vec<Mailbox>>,
    identities: Option<Vec<Identity>>,
    inbox_lists: Vec<(InboxKey, Vec<Email>)>,
    split_counts: Option<(String, HashMap<String, u32>)>,
    body_cache: HashMap<String, Email>,
}

impl PrefetchCache {
    /// Snapshot every account entry to `path` (JSON, 0600, tmp + rename so
    /// a crash mid-write can't leave a torn file). Called by the warmer
    /// after each account pass — mail bodies are user data, so the file
    /// gets the same permissions treatment as the token store.
    pub async fn save_to_disk(&self, path: &std::path::Path) -> Result<(), Error> {
        let mut snapshot: HashMap<String, AccountSnapshot> = HashMap::new();
        let entries: Vec<(String, Arc<Mutex<AccountEntry>>)> = {
            let r = self.inner.read().await;
            r.iter().map(|(k, v)| (k.clone(), v.clone())).collect()
        };
        for (account, entry) in entries {
            let e = entry.lock().await;
            let mut bodies: Vec<(&String, &Email)> = e.body_cache.iter().collect();
            bodies.sort_by_key(|b| std::cmp::Reverse(b.1.received_at));
            snapshot.insert(
                account,
                AccountSnapshot {
                    mailboxes: e.mailboxes.clone(),
                    identities: e.identities.clone(),
                    inbox_lists: e
                        .inbox_lists
                        .iter()
                        .map(|(k, l)| (k.clone(), l.emails.clone()))
                        .collect(),
                    split_counts: e.split_counts.clone(),
                    body_cache: bodies
                        .into_iter()
                        .take(SNAPSHOT_BODY_CAP)
                        .map(|(id, email)| (id.clone(), email.clone()))
                        .collect(),
                },
            );
        }

        let json = serde_json::to_vec(&snapshot)
            .map_err(|e| Error::Internal(format!("Failed to serialize prefetch snapshot: {e}")))?;
        let tmp = path.with_extension("tmp");
        write_snapshot_file(&tmp, &json)?;
        std::fs::rename(&tmp, path)
            .map_err(|e| Error::Internal(format!("Failed to move prefetch snapshot: {e}")))?;
        Ok(())
    }

    /// Build a cache from the snapshot at `path`, marking every restored
    /// inbox list stale. Restores only accounts in `allowed_accounts` so a
    /// removed account's mail doesn't outlive its configuration. Any
    /// read/parse failure just means starting cold — exactly the pre-
    /// persistence behavior.
    pub fn load_from_disk(path: &std::path::Path, allowed_accounts: &[String]) -> Self {
        let cache = Self::new();
        let raw = match std::fs::read(path) {
            Ok(b) => b,
            Err(_) => return cache, // no snapshot yet — normal on first run
        };
        let snapshot: HashMap<String, AccountSnapshot> = match serde_json::from_slice(&raw) {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(
                    "prefetch: ignoring unreadable snapshot {}: {e}",
                    path.display()
                );
                return cache;
            }
        };
        let mut restored = 0usize;
        let mut inner = HashMap::new();
        for (account, snap) in snapshot {
            if !allowed_accounts.contains(&account) {
                continue;
            }
            restored += 1;
            inner.insert(
                account,
                Arc::new(Mutex::new(AccountEntry {
                    mailboxes: snap.mailboxes,
                    identities: snap.identities,
                    inbox_lists: snap
                        .inbox_lists
                        .into_iter()
                        .map(|(k, emails)| {
                            (
                                k,
                                InboxList {
                                    emails,
                                    stale: true,
                                },
                            )
                        })
                        .collect(),
                    split_counts: snap.split_counts,
                    body_cache: snap.body_cache,
                    version: 0,
                })),
            );
        }
        if restored > 0 {
            tracing::info!(
                accounts = restored,
                "prefetch: restored snapshot from {}",
                path.display()
            );
        }
        Self {
            inner: RwLock::new(inner),
        }
    }
}

/// Write with 0600 on Unix — snapshots contain mail bodies. Mirrors the
/// token store's `write_token_file`.
fn write_snapshot_file(path: &std::path::Path, contents: &[u8]) -> Result<(), Error> {
    #[cfg(unix)]
    {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(path)
            .map_err(|e| Error::Internal(format!("Failed to open prefetch snapshot: {e}")))?;
        file.write_all(contents)
            .map_err(|e| Error::Internal(format!("Failed to write prefetch snapshot: {e}")))?;
    }
    #[cfg(not(unix))]
    {
        std::fs::write(path, contents)
            .map_err(|e| Error::Internal(format!("Failed to write prefetch snapshot: {e}")))?;
    }
    Ok(())
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
pub async fn prefetch_account(state: Arc<crate::types::AppState>, account_id: &str) {
    let started = std::time::Instant::now();
    let cache = state.prefetch.clone();

    // Snapshot the version once at the start of the pass: any mutation
    // during the pass discards every subsequent write. At the current
    // 5-minute interval the cost of a cancelled pass is one extra live
    // fetch on the next user click, which is fine. **Revisit if the
    // interval shrinks** — at e.g. 30 s the "cancel the whole pass on
    // any click" bias becomes wasteful and we'd want to re-snapshot
    // between phases so an un-invalidated phase can still land.
    let v = cache.version(account_id).await;

    // --- mailboxes ---
    let mailboxes = match fetch_mailboxes(&state, account_id).await {
        Ok(m) => {
            if cache.try_set_mailboxes(account_id, v, m.clone()).await {
                Some(m)
            } else {
                tracing::debug!(
                    account = %account_id,
                    "prefetch: mailboxes discarded — version changed mid-fetch"
                );
                // Version already changed; the inbox + split-counts
                // writes below would also be rejected. Bail rather
                // than burn ~22 s of Gmail RTT on results we'll throw
                // away.
                return;
            }
        }
        Err(e) => {
            tracing::warn!(account = %account_id, "prefetch: mailboxes failed: {e}");
            None
        }
    };

    // --- identities ---
    match fetch_identities(&state, account_id).await {
        Ok(ids) => {
            if !cache.try_set_identities(account_id, v, ids).await {
                tracing::debug!(
                    account = %account_id,
                    "prefetch: identities discarded — version changed mid-fetch"
                );
                return;
            }
        }
        Err(e) => tracing::warn!(account = %account_id, "prefetch: identities failed: {e}"),
    }

    // --- per-mailbox lists + bodies + split counts ---
    if let Some(mailboxes) = mailboxes {
        warm_all_mailboxes(state.clone(), account_id, v, &mailboxes).await;
    }

    tracing::info!(
        account = %account_id,
        elapsed_ms = started.elapsed().as_millis() as u64,
        "prefetch: account warmed"
    );
    // Snapshot persistence is scheduled by spawn_warmer (first + last
    // account of each pass), not here — saving the full multi-account
    // snapshot after every account rewrote tens of MB of mostly-unchanged
    // JSON several times per cycle (roborev 307 #5).
}

/// Top-N latest bodies to prefetch per mailbox per warm cycle. Sized to match
/// a typical preview window — opening any recent message in any mailbox is a
/// cache hit. If a user runs into Gmail quota issues, lower this first; if
/// memory pressure is the problem, lower this *and* introduce per-cache
/// eviction. Default 25 = ~22 MB resident per warm cycle for a 3-account
/// 6-mailbox-per-account setup (see plan cost-analysis).
pub(crate) const BODY_PREFETCH_PER_MAILBOX: usize = 25;

/// Store one warmed mailbox list (or log its failure). Returns false when
/// the account entry's version changed mid-fetch — a user mutation ran, so
/// the caller should abort the rest of the pass (every later write would be
/// rejected too).
async fn store_warmed_list(
    cache: &PrefetchCache,
    account_id: &str,
    v: u64,
    mailbox_id: &str,
    list_res: Result<Vec<Email>, Error>,
    warmed_ids: &mut Vec<(String, Vec<String>)>,
) -> bool {
    match list_res {
        Ok(emails) => {
            let ids = emails.iter().map(|e| e.id.clone()).collect::<Vec<_>>();
            if !cache
                .try_set_inbox_list(
                    account_id,
                    v,
                    InboxKey {
                        mailbox_id: mailbox_id.to_string(),
                        limit: crate::routes::DEFAULT_INBOX_LIMIT,
                        // The warmer always warms the default (newest-first)
                        // order — it has no notion of a per-session sort
                        // toggle. Non-default-sort requests miss this entry
                        // and fetch live; see the `InboxKey` doc comment.
                        sort: EmailSort::DateDesc,
                    },
                    emails,
                )
                .await
            {
                tracing::debug!(
                    account = %account_id,
                    mailbox = %mailbox_id,
                    "prefetch: inbox list discarded — version changed mid-fetch"
                );
                return false;
            }
            tracing::debug!(
                account = %account_id,
                mailbox = %mailbox_id,
                list_n = ids.len(),
                "warmed mailbox list"
            );
            warmed_ids.push((mailbox_id.to_string(), ids));
        }
        Err(e) => tracing::warn!(
            account = %account_id,
            mailbox = %mailbox_id,
            "mailbox warm failed: {e}"
        ),
    }
    true
}

/// Warm the inbox-role list to completion first, then fan out the remaining
/// mailboxes (and the inbox's split-counts) in parallel. The provider's
/// `RateLimiter` already throttles the fan-out (main pool: 5 concurrent ×
/// 80 ms spacing on Gmail; interactive requests ride a small reserved lane
/// on top — see `RateLimiter`), so it doesn't need its own throttle — but
/// the limiter drains FIFO, which is exactly why the inbox goes first: on a
/// cold start the user is staring at the inbox's disk-restored (stale)
/// snapshot, and its fresh list is the single most urgent write. Mixed into
/// the fan-out it would finish minutes later, interleaved behind every
/// label; alone at the front it lands in seconds. Split-counts join the
/// general fan-out — they feed the tab badges, not the list paint.
async fn warm_all_mailboxes(
    state: Arc<crate::types::AppState>,
    account_id: &str,
    v: u64,
    mailboxes: &[Mailbox],
) {
    let cache = state.prefetch.clone();
    let mut warmed_ids: Vec<(String, Vec<String>)> = Vec::new();

    // ---- Phase 1a: inbox-role list, alone, to completion ----
    let (inbox_role, rest): (Vec<&Mailbox>, Vec<&Mailbox>) = mailboxes
        .iter()
        .partition(|m| m.role.as_deref() == Some("inbox"));
    for mb in &inbox_role {
        let list = fetch_inbox(&state, account_id, &mb.id).await;
        if !store_warmed_list(&cache, account_id, v, &mb.id, list, &mut warmed_ids).await {
            return;
        }
    }

    // The inbox's top bodies too, still ahead of the fan-out: the newest
    // inbox mail is what the user opens first on a cold start, and the
    // body phase used to run dead last — after every label list and the
    // split-count sample — leaving exactly those bodies cold for minutes.
    // Taking the Vec (instead of remembering an offset into it) keeps
    // phase 2 structurally unable to double-warm these entries — it only
    // ever sees what phase 1b appends (roborev 312 #5).
    let inbox_warmed = std::mem::take(&mut warmed_ids);
    let mut total_bodies = 0usize;
    for (mailbox_id, ids) in &inbox_warmed {
        let prefix: Vec<String> = ids
            .iter()
            .take(BODY_PREFETCH_PER_MAILBOX)
            .cloned()
            .collect();
        if prefix.is_empty() {
            continue;
        }
        total_bodies += prefix.len();
        let res = fetch_bodies(&state, account_id, &prefix).await;
        if !store_warmed_bodies(&cache, account_id, v, mailbox_id, res).await {
            return;
        }
    }

    // ---- Phase 1b: remaining mailbox lists + inbox split-counts fan-out ----
    let mut list_set = tokio::task::JoinSet::new();
    for mb in &rest {
        let state = state.clone();
        let account = account_id.to_string();
        let mailbox_id = mb.id.clone();
        list_set.spawn(async move {
            let list = fetch_inbox(&state, &account, &mailbox_id).await;
            (mailbox_id, Some(list), None)
        });
    }
    for mb in &inbox_role {
        // Only warm split-counts for the inbox role — that's the only
        // mailbox where the sidebar split tabs render.
        let state = state.clone();
        let account = account_id.to_string();
        let mailbox_id = mb.id.clone();
        list_set.spawn(async move {
            let counts = fetch_split_counts(&state, &account, &mailbox_id).await;
            (mailbox_id, None, Some(counts))
        });
    }

    while let Some(joined) = list_set.join_next().await {
        let (mailbox_id, list_res, counts_res) = match joined {
            Ok(t) => t,
            Err(je) => {
                tracing::warn!(
                    account = %account_id,
                    "mailbox warm task panicked: {je}"
                );
                continue;
            }
        };
        if let Some(list_res) = list_res
            && !store_warmed_list(
                &cache,
                account_id,
                v,
                &mailbox_id,
                list_res,
                &mut warmed_ids,
            )
            .await
        {
            return;
        }
        if let Some(counts_res) = counts_res {
            match counts_res {
                Ok(counts) => {
                    if !cache
                        .try_set_split_counts(account_id, v, mailbox_id, counts)
                        .await
                    {
                        tracing::debug!(
                            account = %account_id,
                            "prefetch: split-counts discarded — version changed mid-fetch"
                        );
                    }
                }
                Err(e) => {
                    tracing::warn!(account = %account_id, "prefetch: split-counts failed: {e}")
                }
            }
        }
    }

    // ---- Phase 2: top-N body fan-out across the remaining mailboxes ----
    // (warmed_ids holds only phase 1b's entries — the inbox's bodies were
    // taken and warmed in phase 1a)
    let mut body_set = tokio::task::JoinSet::new();
    for (mailbox_id, ids) in warmed_ids {
        let prefix: Vec<String> = ids.into_iter().take(BODY_PREFETCH_PER_MAILBOX).collect();
        if prefix.is_empty() {
            continue;
        }
        total_bodies += prefix.len();
        let state = state.clone();
        let account = account_id.to_string();
        body_set.spawn(async move {
            let res = fetch_bodies(&state, &account, &prefix).await;
            (mailbox_id, res)
        });
    }

    while let Some(joined) = body_set.join_next().await {
        let (mailbox_id, res) = match joined {
            Ok(t) => t,
            Err(je) => {
                tracing::warn!(
                    account = %account_id,
                    "body warm task panicked: {je}"
                );
                continue;
            }
        };
        if !store_warmed_bodies(&cache, account_id, v, &mailbox_id, res).await {
            return;
        }
    }

    tracing::info!(
        account = %account_id,
        bodies = total_bodies,
        "warmed account bodies across all mailboxes"
    );
}

/// Store one mailbox's warmed bodies (or log the failure). Returns false
/// when the account entry's version changed mid-fetch — abort the pass,
/// every later write would be rejected too.
async fn store_warmed_bodies(
    cache: &PrefetchCache,
    account_id: &str,
    v: u64,
    mailbox_id: &str,
    res: Result<Vec<Email>, Error>,
) -> bool {
    match res {
        Ok(emails) => {
            let count = emails.len();
            for email in emails {
                let id = email.id.clone();
                if !cache.try_set_body(account_id, v, id, email).await {
                    tracing::debug!(
                        account = %account_id,
                        "prefetch: body discarded — version changed mid-fetch"
                    );
                    return false;
                }
            }
            tracing::debug!(
                account = %account_id,
                mailbox = %mailbox_id,
                bodies_n = count,
                "warmed mailbox bodies"
            );
        }
        Err(e) => tracing::warn!(
            account = %account_id,
            mailbox = %mailbox_id,
            "body warm failed: {e}"
        ),
    }
    true
}

async fn fetch_bodies(
    state: &crate::types::AppState,
    account_id: &str,
    ids: &[String],
) -> Result<Vec<Email>, Error> {
    let session_lock = session_for(state, account_id).await?;
    // Chunked so the read guard is released between batches — a send (which
    // needs write()) queued mid-warm gets in within one chunk instead of
    // waiting out the whole body fan-out. Bodies are the heaviest payload,
    // hence the smaller chunk. See provider::get_emails_chunked.
    crate::provider::get_emails_chunked(&session_lock, ids, true, None, 10).await
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
    let ids = {
        let session = session_lock.read().await;
        crate::provider::query_emails(
            &session,
            Some(mailbox_id),
            crate::routes::DEFAULT_INBOX_LIMIT,
            0,
            None,
            EmailSort::DateDesc,
        )
        .await?
    };
    // Guard released between chunks so a queued writer (send) isn't stuck
    // behind a whole 150-message fan-out. See provider::get_emails_chunked.
    crate::provider::get_emails_chunked(
        &session_lock,
        &ids,
        false,
        None,
        crate::provider::GET_EMAILS_CHUNK,
    )
    .await
}

async fn fetch_split_counts(
    state: &crate::types::AppState,
    account_id: &str,
    mailbox_id: &str,
) -> Result<HashMap<String, u32>, Error> {
    let config = crate::splits::load_splits(
        &state.splits_config_path,
        std::env::var("SUPERVILLAIN_SPLITS").ok().as_deref(),
    )
    .scoped_to(Some(account_id));
    if config.splits.is_empty() {
        return Ok(HashMap::new());
    }
    // Delegate to the same function the `/api/split-counts` handler
    // calls — drift between warmer and route would mean the cached
    // value disagrees with what the route would have produced on a
    // miss, which then flips visibly to the user every invalidate.
    crate::routes::compute_split_counts(state, Some(account_id), mailbox_id, &config, None).await
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
        let started = std::time::Instant::now();
        tracing::info!(count = n, "prefetch: warming pass starting");
        for account in accounts {
            warm_account(account).await;
        }
        tracing::info!(
            count = n,
            elapsed_ms = started.elapsed().as_millis() as u64,
            "prefetch: warming pass complete"
        );
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
    // Pass-position bookkeeping for snapshot saves. The full multi-account
    // snapshot is saved twice per pass: after the FIRST account (the
    // default — its freshly-warmed inbox is what a mid-cycle kill must not
    // lose; the launcher stops the server with a plain kill) and after the
    // LAST (the complete pass). warm_loop drives accounts sequentially, so
    // the counters never interleave.
    use std::sync::atomic::{AtomicUsize, Ordering};
    let pass_total = std::sync::Arc::new(AtomicUsize::new(0));
    let pass_done = std::sync::Arc::new(AtomicUsize::new(0));
    let total_for_list = pass_total.clone();
    let done_for_list = pass_done.clone();
    tokio::spawn(async move {
        warm_loop(
            move || {
                let s = state_for_list.clone();
                let total = total_for_list.clone();
                let done = done_for_list.clone();
                async move {
                    let reg = s.accounts.read().await;
                    let default = reg.default_account.clone();
                    let mut ids = reg.sessions.keys().cloned().collect::<Vec<_>>();
                    // Warm the default account first: it's the one the UI
                    // shows on a cold open, so its stale disk snapshot is
                    // the first thing the user is waiting to see replaced.
                    // HashMap iteration order would otherwise randomize
                    // which account gets the first (and fastest) slot.
                    ids.sort_by_key(|id| *id != default);
                    total.store(ids.len(), Ordering::SeqCst);
                    done.store(0, Ordering::SeqCst);
                    ids
                }
            },
            interval,
            move |account| {
                let s = state_for_warm.clone();
                let pass_total = pass_total.clone();
                let pass_done = pass_done.clone();
                async move {
                    prefetch_account(s.clone(), &account).await;
                    let done = pass_done.fetch_add(1, Ordering::SeqCst) + 1;
                    if (done == 1 || done == pass_total.load(Ordering::SeqCst))
                        && let Err(e) = s.prefetch.save_to_disk(&s.prefetch_cache_path).await
                    {
                        tracing::warn!(
                            account = %account,
                            "prefetch: snapshot save failed: {e}"
                        );
                    }
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
            in_reply_to: None,
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
            sort: EmailSort::DateDesc,
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
            sort: EmailSort::DateDesc,
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
            sort: EmailSort::DateDesc,
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
            sort: EmailSort::DateDesc,
        };
        let set = cache
            .try_set_inbox_list("acc-1", v0, key.clone(), vec![email("stale")])
            .await;
        assert!(!set);
        assert!(cache.get_inbox_list("acc-1", &key).await.is_none());
    }

    #[tokio::test]
    async fn warmer_inbox_key_matches_route_handler_key() {
        // Regression test for the `is_cacheable` predicate in
        // list_emails: if the route uses one limit while the warmer
        // stores under another, every default account-switch fetch
        // silently bypasses the cache. Today both reference
        // `routes::DEFAULT_INBOX_LIMIT`. This test pins that contract:
        // an inbox_list stored at the constant's value is hit by a
        // lookup that also uses the constant.
        let cache = PrefetchCache::new();
        let key = InboxKey {
            mailbox_id: "INBOX".into(),
            limit: crate::routes::DEFAULT_INBOX_LIMIT,
            sort: EmailSort::DateDesc,
        };
        cache
            .set_inbox_list("acc-1", key.clone(), vec![email("e1")])
            .await;

        // A second lookup using the same constant should hit, not miss.
        let lookup = InboxKey {
            mailbox_id: "INBOX".into(),
            limit: crate::routes::DEFAULT_INBOX_LIMIT,
            sort: EmailSort::DateDesc,
        };
        assert!(
            cache.get_inbox_list("acc-1", &lookup).await.is_some(),
            "warmer InboxKey and route InboxKey must share DEFAULT_INBOX_LIMIT"
        );
    }

    #[tokio::test]
    async fn invalidate_forces_next_or_fetch_call_to_refetch() {
        // Locks in the route-level contract that mutation handlers
        // currently satisfy by calling state.prefetch.invalidate(&id):
        // once invalidated, the *next* *_or_fetch call must invoke the
        // live closure (i.e. re-hit the provider), not return cached
        // pre-mutation data. If anyone drops one of the invalidate()
        // lines from a mutation handler this test still passes — but
        // if the cache itself ever grows a "soft invalidation" mode
        // that retains stale data past invalidate(), this test fails.
        use std::sync::atomic::{AtomicU32, Ordering};
        let cache = PrefetchCache::new();
        let calls = Arc::new(AtomicU32::new(0));

        // Populate
        let c2 = calls.clone();
        let _ = cache
            .mailboxes_or_fetch("acc-1", move || {
                let c2 = c2.clone();
                async move {
                    c2.fetch_add(1, Ordering::SeqCst);
                    Ok(vec![mb("inbox")])
                }
            })
            .await
            .unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 1);

        // Simulate a mutation handler firing.
        cache.invalidate("acc-1").await;

        // Next read must hit the provider, not return the cached vec.
        let c3 = calls.clone();
        let got = cache
            .mailboxes_or_fetch("acc-1", move || {
                let c3 = c3.clone();
                async move {
                    c3.fetch_add(1, Ordering::SeqCst);
                    Ok(vec![mb("inbox-fresh")])
                }
            })
            .await
            .unwrap();
        assert_eq!(
            calls.load(Ordering::SeqCst),
            2,
            "post-invalidate read must call the live closure"
        );
        assert_eq!(got[0].id, "inbox-fresh");
    }

    #[tokio::test(start_paused = true)]
    async fn warm_loop_runs_initial_pass_then_repeats_each_interval() {
        use std::sync::atomic::{AtomicU32, Ordering};
        let calls = Arc::new(AtomicU32::new(0));
        let interval = std::time::Duration::from_secs(300);

        // Channel-based barrier: the warm closure signals on every
        // invocation, so the test never has to guess about yield
        // ordering — it just `recv().await`s the next signal. Beats
        // the previous "sleep(1ms) to nudge the runtime" pattern,
        // which silently broke whenever the inner loop's await
        // ordering shifted.
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<()>();
        let cls = calls.clone();
        let handle = tokio::spawn(async move {
            warm_loop(
                || async { vec!["acc-1".to_string()] },
                interval,
                move |_acc| {
                    let c = cls.clone();
                    let tx = tx.clone();
                    async move {
                        c.fetch_add(1, Ordering::SeqCst);
                        let _ = tx.send(());
                    }
                },
            )
            .await;
        });

        // Initial pass fires ~200 ms after spawn.
        tokio::time::advance(std::time::Duration::from_millis(250)).await;
        rx.recv().await.expect("initial pass should have fired");
        assert_eq!(calls.load(Ordering::SeqCst), 1);

        // Subsequent pass after one interval.
        tokio::time::advance(interval + std::time::Duration::from_millis(1)).await;
        rx.recv().await.expect("second pass should have fired");
        assert_eq!(calls.load(Ordering::SeqCst), 2);

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
            sort: EmailSort::DateDesc,
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
            sort: EmailSort::DateDesc,
        };
        assert!(cache.get_inbox_list("acc-1", &k50).await.is_none());

        // Different mailbox: miss
        let k_other = InboxKey {
            mailbox_id: "archive".into(),
            limit: 150,
            sort: EmailSort::DateDesc,
        };
        assert!(cache.get_inbox_list("acc-1", &k_other).await.is_none());
    }

    #[tokio::test]
    async fn inbox_list_keyed_by_sort_too() {
        // Regression test (kata 09ef): before `sort` joined `InboxKey`, a
        // DateAsc request and the warmer's always-DateDesc entry shared the
        // same (mailbox_id, limit) slot. Whichever wrote last would then be
        // served to *both* — e.g. a warmed desc list could be handed back
        // to a request that explicitly asked for ascending order, wrong
        // order served silently rather than the plan's required error/
        // documented-limitation path. `sort` in the key keeps them in
        // separate slots.
        //
        // As of roborev 291, `routes::list_is_cacheable` also stops
        // `list_emails` from ever calling `set_inbox_list`/`get_inbox_list`
        // with a non-`DateDesc` sort in the first place (see
        // `routes::list_is_cacheable_false_for_date_asc_sort`), so an
        // `asc_key` entry can no longer actually appear in production. This
        // test still pins the raw cache primitive's keying as a
        // defense-in-depth invariant: *if* something above this layer ever
        // did write a non-default-sort entry, it still couldn't collide
        // with or clobber the default-sort one.
        let cache = PrefetchCache::new();
        let desc_key = InboxKey {
            mailbox_id: "inbox".into(),
            limit: 150,
            sort: EmailSort::DateDesc,
        };
        let asc_key = InboxKey {
            mailbox_id: "inbox".into(),
            limit: 150,
            sort: EmailSort::DateAsc,
        };

        cache
            .set_inbox_list("acc-1", desc_key.clone(), vec![email("newest-first")])
            .await;

        // Same mailbox/limit, different sort: must miss, not return the
        // desc-ordered payload under a different name.
        assert!(
            cache.get_inbox_list("acc-1", &asc_key).await.is_none(),
            "an asc lookup must not be served the desc-keyed entry"
        );

        cache
            .set_inbox_list("acc-1", asc_key.clone(), vec![email("oldest-first")])
            .await;

        // Both entries coexist independently — storing asc must not have
        // clobbered desc.
        assert_eq!(
            cache.get_inbox_list("acc-1", &desc_key).await.unwrap()[0].id,
            "newest-first"
        );
        assert_eq!(
            cache.get_inbox_list("acc-1", &asc_key).await.unwrap()[0].id,
            "oldest-first"
        );
    }

    #[tokio::test]
    async fn inbox_lists_holds_two_mailbox_entries_simultaneously() {
        // The warmer warms a list per *every* mailbox, not just inbox. So
        // the cache must keep both entries — storing Archive must not
        // evict Inbox.
        let cache = PrefetchCache::new();
        let inbox_key = InboxKey {
            mailbox_id: "inbox".into(),
            limit: 150,
            sort: EmailSort::DateDesc,
        };
        let archive_key = InboxKey {
            mailbox_id: "archive".into(),
            limit: 150,
            sort: EmailSort::DateDesc,
        };
        cache
            .set_inbox_list("acc-1", inbox_key.clone(), vec![email("a")])
            .await;
        cache
            .set_inbox_list("acc-1", archive_key.clone(), vec![email("b")])
            .await;

        assert!(
            cache.get_inbox_list("acc-1", &inbox_key).await.is_some(),
            "Inbox entry must survive a subsequent Archive store"
        );
        assert!(
            cache.get_inbox_list("acc-1", &archive_key).await.is_some(),
            "Archive entry must coexist with Inbox"
        );
    }

    #[tokio::test]
    async fn body_cache_roundtrip() {
        let cache = PrefetchCache::new();
        assert!(cache.get_body("acc-1", "missing").await.is_none());
        cache
            .set_body("acc-1", "msg-1".into(), email("msg-1"))
            .await;
        let got = cache.get_body("acc-1", "msg-1").await.unwrap();
        assert_eq!(got.id, "msg-1");
    }

    #[tokio::test]
    async fn body_or_fetch_skips_fallback_on_hit() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let cache = PrefetchCache::new();
        cache
            .set_body("acc-1", "msg-1".into(), email("msg-1"))
            .await;
        let calls = Arc::new(AtomicUsize::new(0));
        let calls_for_closure = calls.clone();
        let got = cache
            .body_or_fetch("acc-1", "msg-1", move || {
                let c = calls_for_closure.clone();
                async move {
                    c.fetch_add(1, Ordering::SeqCst);
                    Ok(email("should-not-fire"))
                }
            })
            .await
            .unwrap();
        assert_eq!(got.id, "msg-1");
        assert_eq!(
            calls.load(Ordering::SeqCst),
            0,
            "fallback must not run on cache hit"
        );
    }

    #[tokio::test]
    async fn body_or_fetch_populates_on_miss() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let cache = PrefetchCache::new();
        let calls = Arc::new(AtomicUsize::new(0));
        let calls_for_closure = calls.clone();
        let got = cache
            .body_or_fetch("acc-1", "msg-1", move || {
                let c = calls_for_closure.clone();
                async move {
                    c.fetch_add(1, Ordering::SeqCst);
                    Ok(email("msg-1"))
                }
            })
            .await
            .unwrap();
        assert_eq!(got.id, "msg-1");
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        // Subsequent call: cached, fallback doesn't fire
        let _ = cache
            .body_or_fetch("acc-1", "msg-1", || async {
                panic!("must not run after first miss populates")
            })
            .await
            .unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn invalidate_clears_lists_and_counts_but_not_bodies() {
        // Per-mutation invalidate must NOT drain body_cache — bodies are
        // content-only (text/html), unaffected by mark-read/archive/move.
        // Wiping them on every mutation defeats the prefetch entirely.
        let cache = PrefetchCache::new();
        cache.set_mailboxes("acc-1", vec![mb("inbox")]).await;
        let key = InboxKey {
            mailbox_id: "inbox".into(),
            limit: 150,
            sort: EmailSort::DateDesc,
        };
        cache
            .set_inbox_list("acc-1", key.clone(), vec![email("e1")])
            .await;
        cache
            .set_body("acc-1", "msg-1".into(), email("msg-1"))
            .await;

        cache.invalidate("acc-1").await;

        assert!(cache.get_mailboxes("acc-1").await.is_none());
        assert!(cache.get_inbox_list("acc-1", &key).await.is_none());
        assert!(
            cache.get_body("acc-1", "msg-1").await.is_some(),
            "body_cache must survive a per-mutation invalidate"
        );
    }

    #[tokio::test]
    async fn invalidate_full_clears_everything_including_bodies() {
        let cache = PrefetchCache::new();
        cache.set_mailboxes("acc-1", vec![mb("inbox")]).await;
        cache
            .set_body("acc-1", "msg-1".into(), email("msg-1"))
            .await;
        let v0 = cache.version("acc-1").await;

        cache.invalidate_full("acc-1").await;

        assert!(cache.get_mailboxes("acc-1").await.is_none());
        assert!(
            cache.get_body("acc-1", "msg-1").await.is_none(),
            "invalidate_full must drain body_cache"
        );
        assert_eq!(cache.version("acc-1").await, v0 + 1);
    }

    #[tokio::test]
    async fn invalidate_clears_all_fields_and_bumps_version() {
        let cache = PrefetchCache::new();
        let key = InboxKey {
            mailbox_id: "inbox".into(),
            limit: 150,
            sort: EmailSort::DateDesc,
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
    async fn invalidate_split_counts_clears_only_counts_and_bumps_version() {
        let cache = PrefetchCache::new();
        let key = InboxKey {
            mailbox_id: "inbox".into(),
            limit: 150,
            sort: EmailSort::DateDesc,
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
        cache.invalidate_split_counts("acc-1").await;

        assert!(
            cache.get_split_counts("acc-1", "inbox").await.is_none(),
            "split-counts must be cleared"
        );
        assert!(
            cache.get_mailboxes("acc-1").await.is_some(),
            "mailboxes must survive a split-counts-only invalidate"
        );
        assert!(
            cache.get_identities("acc-1").await.is_some(),
            "identities must survive a split-counts-only invalidate"
        );
        assert!(
            cache.get_inbox_list("acc-1", &key).await.is_some(),
            "inbox_lists must survive a split-counts-only invalidate"
        );
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

    // ---- Disk persistence ----

    fn snapshot_path(name: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!(
            "sv-prefetch-test-{}-{name}.json",
            std::process::id()
        ))
    }

    #[tokio::test]
    async fn snapshot_roundtrip_restores_data_and_marks_lists_stale() {
        let cache = PrefetchCache::new();
        let key = InboxKey {
            mailbox_id: "inbox".into(),
            limit: 150,
            sort: EmailSort::DateDesc,
        };
        cache.set_mailboxes("acc-1", vec![mb("inbox")]).await;
        cache.set_identities("acc-1", vec![ident("me")]).await;
        cache
            .set_inbox_list("acc-1", key.clone(), vec![email("e1")])
            .await;
        cache.set_body("acc-1", "e1".into(), email("e1")).await;
        let mut counts = HashMap::new();
        counts.insert("split-a".into(), 3);
        cache
            .set_split_counts("acc-1", "inbox".into(), counts)
            .await;

        let path = snapshot_path("roundtrip");
        cache.save_to_disk(&path).await.unwrap();
        let restored = PrefetchCache::load_from_disk(&path, &["acc-1".to_string()]);
        std::fs::remove_file(&path).ok();

        // Everything comes back...
        assert_eq!(
            restored.get_mailboxes("acc-1").await.unwrap()[0].id,
            "inbox"
        );
        assert_eq!(restored.get_identities("acc-1").await.unwrap()[0].id, "me");
        assert_eq!(restored.get_body("acc-1", "e1").await.unwrap().id, "e1");
        assert_eq!(
            restored
                .get_split_counts("acc-1", "inbox")
                .await
                .unwrap()
                .get("split-a"),
            Some(&3)
        );
        // ...and the inbox list is served as a hit, but flagged stale so
        // the route can tell the frontend to re-poll.
        let (emails, stale) = restored
            .inbox_list_or_fetch("acc-1", key.clone(), || async {
                panic!("stale hit must not trigger a live fetch")
            })
            .await
            .unwrap();
        assert_eq!(emails[0].id, "e1");
        assert!(stale, "disk-restored lists must be marked stale");

        // A live write (what the warmer does) clears the flag.
        let v = restored.version("acc-1").await;
        assert!(
            restored
                .try_set_inbox_list("acc-1", v, key.clone(), vec![email("e2")])
                .await
        );
        let (emails, stale) = restored
            .inbox_list_or_fetch("acc-1", key, || async {
                panic!("fresh hit must not trigger a live fetch")
            })
            .await
            .unwrap();
        assert_eq!(emails[0].id, "e2");
        assert!(!stale, "a warmer write must clear the stale flag");
    }

    #[tokio::test]
    async fn fresh_writes_are_not_stale() {
        let cache = PrefetchCache::new();
        let key = InboxKey {
            mailbox_id: "inbox".into(),
            limit: 150,
            sort: EmailSort::DateDesc,
        };
        cache
            .set_inbox_list("acc-1", key.clone(), vec![email("e1")])
            .await;
        let (_, stale) = cache
            .inbox_list_or_fetch("acc-1", key, || async { unreachable!() })
            .await
            .unwrap();
        assert!(!stale, "a live set_inbox_list must never read back stale");
    }

    #[tokio::test]
    async fn snapshot_load_skips_accounts_no_longer_configured() {
        let cache = PrefetchCache::new();
        cache.set_mailboxes("acc-keep", vec![mb("a")]).await;
        cache.set_mailboxes("acc-removed", vec![mb("b")]).await;

        let path = snapshot_path("skip-removed");
        cache.save_to_disk(&path).await.unwrap();
        let restored = PrefetchCache::load_from_disk(&path, &["acc-keep".to_string()]);
        std::fs::remove_file(&path).ok();

        assert!(restored.get_mailboxes("acc-keep").await.is_some());
        assert!(
            restored.get_mailboxes("acc-removed").await.is_none(),
            "a removed account's mail must not outlive its configuration"
        );
    }

    #[tokio::test]
    async fn snapshot_load_missing_or_garbage_file_starts_cold() {
        let missing = PrefetchCache::load_from_disk(
            std::path::Path::new("/nonexistent/prefetch.json"),
            &["acc-1".to_string()],
        );
        assert!(missing.get_mailboxes("acc-1").await.is_none());

        let path = snapshot_path("garbage");
        std::fs::write(&path, b"not json{{{").unwrap();
        let garbage = PrefetchCache::load_from_disk(&path, &["acc-1".to_string()]);
        std::fs::remove_file(&path).ok();
        assert!(garbage.get_mailboxes("acc-1").await.is_none());
    }
}
