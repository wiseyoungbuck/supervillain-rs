//! Open tracking for sent email (kata e2h4).
//!
//! A unique pixel is injected into outgoing HTML bodies at send time; the
//! public `/t/{token}.gif` route (deliberately OUTSIDE `/api` — that prefix
//! is private per api.js's scoping regex and the mobile SW's cache skip)
//! serves a 1×1 gif and records each fetch as an open event. Tracking is
//! inert unless the operator sets `SUPERVILLAIN_TRACKING_BASE` to the
//! server's publicly reachable URL — without an internet-facing base the
//! pixel could never load, so no pixel is injected and nothing is recorded.
//!
//! Injection happens exactly once, in the send handler, so the immediate
//! and deferred (scheduled-send daemon) paths share one pixel; the injector
//! is idempotent regardless, keyed on the pixel's own URL shape.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

use crate::error::Error;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TrackedSend {
    /// Pixel token (32 lowercase hex chars) — the `/t/{token}.gif` key.
    pub token: String,
    pub account_id: String,
    /// Provider message id, filled in after dispatch when the provider
    /// returns one (Graph doesn't) — lets the client join read statuses
    /// onto the Sent list.
    pub email_id: Option<String>,
    pub subject: String,
    pub recipients: Vec<String>,
    pub sent_at: DateTime<Utc>,
    /// One entry per pixel fetch, oldest first.
    pub opens: Vec<DateTime<Utc>>,
}

#[derive(Clone)]
pub struct TrackingStore {
    path: PathBuf,
    records: Arc<RwLock<HashMap<String, TrackedSend>>>,
}

impl std::fmt::Debug for TrackingStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TrackingStore")
            .field("path", &self.path)
            .field("records", &self.records())
            .finish()
    }
}

impl TrackingStore {
    pub fn new(path: PathBuf) -> Self {
        Self {
            path,
            records: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    pub fn load(path: impl AsRef<Path>) -> Self {
        let store = Self::new(path.as_ref().to_path_buf());
        let Ok(contents) = std::fs::read_to_string(path) else {
            return store;
        };
        let parsed: Vec<TrackedSend> = match serde_json::from_str(&contents) {
            Ok(value) => value,
            Err(error) => {
                tracing::warn!("Failed to load tracking.json: {error}");
                return store;
            }
        };
        let mut records = store.records.write().expect("tracking store lock poisoned");
        for record in parsed {
            records.insert(record.token.clone(), record);
        }
        drop(records);
        store
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn insert(&self, record: TrackedSend) {
        self.records
            .write()
            .expect("tracking store lock poisoned")
            .insert(record.token.clone(), record);
    }

    pub fn get(&self, token: &str) -> Option<TrackedSend> {
        self.records
            .read()
            .expect("tracking store lock poisoned")
            .get(token)
            .cloned()
    }

    /// All records, most recently sent first (the display order).
    pub fn records(&self) -> Vec<TrackedSend> {
        let mut records: Vec<_> = self
            .records
            .read()
            .expect("tracking store lock poisoned")
            .values()
            .cloned()
            .collect();
        records.sort_by_key(|record| std::cmp::Reverse(record.sent_at));
        records
    }

    pub fn records_for_account(&self, account_id: &str) -> Vec<TrackedSend> {
        self.records()
            .into_iter()
            .filter(|record| record.account_id == account_id)
            .collect()
    }

    /// Append one open event. Returns false for an unknown token (the
    /// pixel route still serves the gif either way — validity must not
    /// leak to whoever fetches).
    pub fn record_open(&self, token: &str, at: DateTime<Utc>) -> bool {
        let mut records = self.records.write().expect("tracking store lock poisoned");
        match records.get_mut(token) {
            Some(record) => {
                record.opens.push(at);
                true
            }
            None => false,
        }
    }

    /// Attach the provider's message id once dispatch reports it.
    pub fn set_email_id(&self, token: &str, email_id: &str) -> bool {
        let mut records = self.records.write().expect("tracking store lock poisoned");
        match records.get_mut(token) {
            Some(record) => {
                record.email_id = Some(email_id.to_string());
                true
            }
            None => false,
        }
    }

    pub fn save(&self) -> Result<(), Error> {
        let records = self.records();
        let json = serde_json::to_string_pretty(&records)?;
        crate::accounts::atomic_write_bytes(&self.path, json.as_bytes(), false)?;
        Ok(())
    }
}

/// Fresh pixel token: 32 lowercase hex chars (uuid v4, simple format) —
/// the shape `extract_token` scans for.
pub fn new_token() -> String {
    uuid::Uuid::new_v4().simple().to_string()
}

/// Append the tracking pixel to an HTML body. Ammonia-safe attributes only
/// (`sanitize_outgoing_html` runs before injection, but a pixel that would
/// not survive it is a footgun). Inserted before `</body>` when present,
/// appended otherwise.
pub fn inject_pixel(html: &str, base: &str, token: &str) -> String {
    let base = base.trim_end_matches('/');
    // src/width/height/alt only — all on ammonia's default img allowlist,
    // so the pixel survives even if a refactor moves injection before the
    // outgoing sanitizer. Clients hide 1×1 images on their own; a style
    // attribute would be stripped anyway.
    let img = format!(r#"<img src="{base}/t/{token}.gif" width="1" height="1" alt="">"#);
    match html.rfind("</body>") {
        Some(pos) => format!("{}{}{}", &html[..pos], img, &html[pos..]),
        None => format!("{html}{img}"),
    }
}

/// Find an already-injected pixel's token: `/t/` + 32 hex + `.gif`.
pub fn extract_token(html: &str) -> Option<String> {
    let mut rest = html;
    while let Some(idx) = rest.find("/t/") {
        let after = &rest[idx + 3..];
        if after.len() >= 36
            && after.as_bytes()[..32].iter().all(|b| b.is_ascii_hexdigit())
            && after[32..].starts_with(".gif")
        {
            return Some(after[..32].to_string());
        }
        rest = after;
    }
    None
}

pub fn has_pixel(html: &str) -> bool {
    extract_token(html).is_some()
}

/// Inject a pixel into `submission` (HTML bodies only) and record the send.
/// Idempotent: an already-pixeled body (a re-entry, a daemon re-dispatch)
/// is left untouched and no duplicate record is created.
pub fn track_outgoing(
    store: &TrackingStore,
    base: &str,
    account_id: &str,
    submission: &mut crate::types::EmailSubmission,
) -> Result<(), Error> {
    let Some(html) = submission.html_body.as_deref() else {
        return Ok(());
    };
    if has_pixel(html) {
        return Ok(());
    }
    let token = new_token();
    submission.html_body = Some(inject_pixel(html, base, &token));
    let mut recipients = submission.to.clone();
    recipients.extend(submission.cc.iter().cloned());
    store.insert(TrackedSend {
        token,
        account_id: account_id.to_string(),
        email_id: None,
        subject: submission.subject.clone(),
        recipients,
        sent_at: Utc::now(),
        opens: Vec::new(),
    });
    store.save()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::EmailSubmission;

    fn submission(html: Option<&str>) -> EmailSubmission {
        EmailSubmission {
            to: vec!["dest@example.com".into()],
            cc: vec!["cc@example.com".into()],
            subject: "tracked hello".into(),
            text_body: "plain".into(),
            bcc: None,
            html_body: html.map(String::from),
            in_reply_to: None,
            references: None,
            attachments: Vec::new(),
            calendar_ics: None,
            send_at: None,
        }
    }

    const BASE: &str = "https://mail.example.com";

    // Store backed by a per-test tempdir: track_outgoing() persists via
    // save(), and a fixed /tmp path would leak the file after the run and
    // collide with other users' test runs on a shared machine. The TempDir
    // must stay bound in the test so the dir outlives the store.
    fn temp_store() -> (tempfile::TempDir, TrackingStore) {
        let dir = tempfile::tempdir().unwrap();
        let store = TrackingStore::new(dir.path().join("tracking.json"));
        (dir, store)
    }

    #[test]
    fn injected_pixel_is_unique_per_send() {
        let (_dir, store) = temp_store();
        let mut first = submission(Some("<p>hi</p>"));
        let mut second = submission(Some("<p>hi</p>"));
        track_outgoing(&store, BASE, "acct", &mut first).unwrap();
        track_outgoing(&store, BASE, "acct", &mut second).unwrap();
        let t1 = extract_token(first.html_body.as_deref().unwrap())
            .expect("first send must carry a pixel token");
        let t2 = extract_token(second.html_body.as_deref().unwrap())
            .expect("second send must carry a pixel token");
        assert_ne!(t1, t2, "every send gets its own token");
        assert_eq!(t1.len(), 32);
        assert!(t1.chars().all(|c| c.is_ascii_hexdigit()));
        assert!(
            first
                .html_body
                .as_deref()
                .unwrap()
                .contains(&format!("{BASE}/t/{t1}.gif")),
            "pixel URL must be absolute under the configured base"
        );
        assert_eq!(store.records().len(), 2, "each send is recorded once");
    }

    #[test]
    fn injection_is_idempotent_across_redispatch() {
        // The daemon may re-enter the send path for a queued submission that
        // was already pixeled at enqueue time — the body must not grow a
        // second pixel and the store must not grow a second record.
        let (_dir, store) = temp_store();
        let mut sub = submission(Some("<p>hi</p>"));
        track_outgoing(&store, BASE, "acct", &mut sub).unwrap();
        let once = sub.html_body.clone().unwrap();
        track_outgoing(&store, BASE, "acct", &mut sub).unwrap();
        assert_eq!(
            sub.html_body.as_deref().unwrap(),
            once,
            "re-tracking an already-pixeled body must change nothing"
        );
        assert_eq!(
            once.matches("/t/").count(),
            1,
            "exactly one pixel in the body"
        );
        assert_eq!(store.records().len(), 1, "no duplicate record either");
    }

    #[test]
    fn text_only_sends_are_never_tracked() {
        let (_dir, store) = temp_store();
        let mut sub = submission(None);
        track_outgoing(&store, BASE, "acct", &mut sub).unwrap();
        assert_eq!(sub.html_body, None, "no HTML body may be fabricated");
        assert!(store.records().is_empty());
    }

    #[test]
    fn pixel_lands_inside_body_when_the_tag_exists() {
        let html = "<html><body><p>hi</p></body></html>";
        let out = inject_pixel(html, BASE, "0123456789abcdef0123456789abcdef");
        let pixel_at = out.find("/t/").expect("pixel injected");
        let body_close = out.rfind("</body>").unwrap();
        assert!(
            pixel_at < body_close,
            "pixel must sit before </body>, not after the document"
        );
        assert_eq!(
            extract_token(&out).unwrap(),
            "0123456789abcdef0123456789abcdef"
        );
    }

    #[test]
    fn pixel_survives_the_outgoing_sanitizer() {
        // Injection runs after sanitize_outgoing_html in the handler, but a
        // pixel built from non-allowlisted attributes would be one refactor
        // away from silently vanishing. Keep it ammonia-clean.
        let out = inject_pixel("<p>hi</p>", BASE, "0123456789abcdef0123456789abcdef");
        let sanitized = ammonia::clean(&out);
        assert_eq!(
            extract_token(&sanitized).as_deref(),
            Some("0123456789abcdef0123456789abcdef"),
            "the pixel must survive ammonia's default allowlist"
        );
    }

    #[test]
    fn extract_token_ignores_lookalikes() {
        assert_eq!(extract_token("<p>see /t/short.gif</p>"), None);
        assert_eq!(
            extract_token("<p>/t/0123456789abcdef0123456789abcdef.png</p>"),
            None,
            "wrong extension is not a pixel"
        );
        assert_eq!(extract_token("<p>no pixel here</p>"), None);
        let html = format!(
            "<p>/t/tooshort.gif then a real one</p>\
             <img src=\"{BASE}/t/aaaabbbbccccddddeeeeffff00001111.gif\">"
        );
        assert_eq!(
            extract_token(&html).as_deref(),
            Some("aaaabbbbccccddddeeeeffff00001111"),
            "scanning must skip lookalikes and find the real token"
        );
    }

    #[test]
    fn opens_append_per_event_and_unknown_tokens_record_nothing() {
        let (_dir, store) = temp_store();
        let mut sub = submission(Some("<p>hi</p>"));
        track_outgoing(&store, BASE, "acct", &mut sub).unwrap();
        let token = extract_token(sub.html_body.as_deref().unwrap()).unwrap();
        let t1 = Utc::now();
        let t2 = t1 + chrono::Duration::minutes(5);
        assert!(store.record_open(&token, t1));
        assert!(store.record_open(&token, t2));
        assert!(!store.record_open("ffffffffffffffffffffffffffffffff", t1));
        let record = store.get(&token).unwrap();
        assert_eq!(record.opens, vec![t1, t2], "one event per fetch, in order");
    }

    #[test]
    fn email_id_attaches_after_dispatch() {
        let (_dir, store) = temp_store();
        let mut sub = submission(Some("<p>hi</p>"));
        track_outgoing(&store, BASE, "acct", &mut sub).unwrap();
        let token = extract_token(sub.html_body.as_deref().unwrap()).unwrap();
        assert!(store.set_email_id(&token, "M123"));
        assert_eq!(store.get(&token).unwrap().email_id.as_deref(), Some("M123"));
        assert!(!store.set_email_id("ffffffffffffffffffffffffffffffff", "M9"));
    }

    #[test]
    fn store_persists_and_reloads() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tracking.json");
        let store = TrackingStore::new(path.clone());
        let mut sub = submission(Some("<p>hi</p>"));
        track_outgoing(&store, BASE, "acct", &mut sub).unwrap();
        let token = extract_token(sub.html_body.as_deref().unwrap()).unwrap();
        store.record_open(&token, Utc::now());
        store.save().unwrap();
        let loaded = TrackingStore::load(path);
        assert_eq!(loaded.records(), store.records());
    }

    #[test]
    fn records_list_most_recent_first() {
        let (_dir, store) = temp_store();
        let now = Utc::now();
        for (token, age_mins) in [("a", 30i64), ("b", 10), ("c", 20)] {
            store.insert(TrackedSend {
                token: token.repeat(32),
                account_id: "acct".into(),
                email_id: None,
                subject: token.into(),
                recipients: vec![],
                sent_at: now - chrono::Duration::minutes(age_mins),
                opens: vec![],
            });
        }
        let subjects: Vec<_> = store.records().into_iter().map(|r| r.subject).collect();
        assert_eq!(subjects, vec!["b", "c", "a"]);
    }

    #[test]
    fn perf_injection_into_1mb_html_under_budget() {
        // Perf budget from the plan table: injection into 1MB HTML < 20ms.
        // Best-of-5 to strip scheduler noise (same rationale as the
        // scheduled-send scan budget). Synthetic body, no I/O.
        let html = format!(
            "<html><body>{}</body></html>",
            "<p>chunk</p>".repeat(90_000)
        );
        assert!(html.len() > 1_000_000);
        let mut best = std::time::Duration::MAX;
        for _ in 0..5 {
            let start = std::time::Instant::now();
            let out = inject_pixel(&html, BASE, "0123456789abcdef0123456789abcdef");
            best = best.min(start.elapsed());
            assert!(has_pixel(&out), "the timed operation must actually inject");
        }
        assert!(
            best < std::time::Duration::from_millis(20),
            "1MB injection took {best:?} (budget 20ms)"
        );
    }
}
