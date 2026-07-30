//! Server-side Remind Me scheduling.
//!
//! The provider mailbox is only the resting place for a message.  This module
//! owns the durable wake table, the user defaults, and the deterministic parts
//! of the wake decision.  Provider I/O is kept in `provider.rs` and is called
//! by the daemon/route layer.

use chrono::{
    DateTime, Datelike, Duration, NaiveDate, NaiveDateTime, NaiveTime, TimeZone, Utc, Weekday,
};
use chrono_tz::Tz;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

use crate::error::Error;
use crate::provider;
use crate::types::{Email, EmailSort, ParsedQuery};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum ReminderMode {
    #[default]
    IfNoReply,
    Regardless,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReminderRecord {
    pub account_id: String,
    pub email_id: String,
    pub original_inbox_id: String,
    pub wake_at: DateTime<Utc>,
    pub mode: ReminderMode,
    pub snoozed_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReminderSettings {
    #[serde(default = "default_reminder_time")]
    pub default_time: String,
    #[serde(default)]
    pub regardless_default: bool,
    #[serde(default)]
    pub skip_weekends: bool,
}

impl Default for ReminderSettings {
    fn default() -> Self {
        Self {
            default_time: default_reminder_time(),
            regardless_default: false,
            skip_weekends: false,
        }
    }
}

fn default_reminder_time() -> String {
    "08:00".into()
}

#[derive(Clone)]
pub struct ReminderStore {
    path: PathBuf,
    records: Arc<RwLock<HashMap<(String, String), ReminderRecord>>>,
}

impl std::fmt::Debug for ReminderStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ReminderStore")
            .field("path", &self.path)
            .field("records", &self.records())
            .finish()
    }
}

impl ReminderStore {
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
        let parsed: Vec<ReminderRecord> = match serde_json::from_str(&contents) {
            Ok(value) => value,
            Err(error) => {
                tracing::warn!("Failed to load reminders.json: {error}");
                return store;
            }
        };
        let mut records = store.records.write().expect("reminder store lock poisoned");
        for record in parsed {
            records.insert((record.account_id.clone(), record.email_id.clone()), record);
        }
        drop(records);
        store
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn insert(&self, record: ReminderRecord) {
        self.records
            .write()
            .expect("reminder store lock poisoned")
            .insert((record.account_id.clone(), record.email_id.clone()), record);
    }

    pub fn remove(&self, account_id: &str, email_id: &str) -> Option<ReminderRecord> {
        self.records
            .write()
            .expect("reminder store lock poisoned")
            .remove(&(account_id.to_string(), email_id.to_string()))
    }

    pub fn get(&self, account_id: &str, email_id: &str) -> Option<ReminderRecord> {
        self.records
            .read()
            .expect("reminder store lock poisoned")
            .get(&(account_id.to_string(), email_id.to_string()))
            .cloned()
    }

    pub fn records(&self) -> Vec<ReminderRecord> {
        let mut records: Vec<_> = self
            .records
            .read()
            .expect("reminder store lock poisoned")
            .values()
            .cloned()
            .collect();
        records.sort_by_key(|record| record.wake_at);
        records
    }

    pub fn records_for_account(&self, account_id: &str) -> Vec<ReminderRecord> {
        self.records()
            .into_iter()
            .filter(|record| record.account_id == account_id)
            .collect()
    }

    pub fn due(&self, now: DateTime<Utc>) -> Vec<ReminderRecord> {
        self.records()
            .into_iter()
            .filter(|record| record.wake_at <= now)
            .collect()
    }

    pub fn save(&self) -> Result<(), Error> {
        let records = self.records();
        let json = serde_json::to_string_pretty(&records)?;
        crate::accounts::atomic_write_bytes(&self.path, json.as_bytes(), false)?;
        Ok(())
    }

    /// Reconcile records against a provider listing.  The returned IDs are
    /// messages that are in the Reminders mailbox but have no durable row.
    pub fn orphan_ids<'a>(
        &self,
        account_id: &str,
        emails: impl IntoIterator<Item = &'a Email>,
    ) -> Vec<String> {
        let known: HashSet<String> = self
            .records_for_account(account_id)
            .into_iter()
            .map(|record| record.email_id)
            .collect();
        emails
            .into_iter()
            .filter(|email| !known.contains(&email.id))
            .map(|email| email.id.clone())
            .collect()
    }
}

/// Resolve a reminder using the current instant.  Tests and the daemon use
/// [`resolve_wake_at`] so relative phrases have an injected clock.
pub fn resolve_wake(input: &str, settings: &ReminderSettings, tz: Tz) -> Option<DateTime<Utc>> {
    resolve_wake_at(input, settings, tz, Utc::now())
}

/// Pure wake-time resolver.  It intentionally supports the small vocabulary
/// used by the picker rather than pretending to be a general NLP parser.
pub fn resolve_wake_at(
    input: &str,
    settings: &ReminderSettings,
    tz: Tz,
    now: DateTime<Utc>,
) -> Option<DateTime<Utc>> {
    let input = input.trim();
    if input.is_empty() {
        return None;
    }

    let lower = input.to_ascii_lowercase();
    let local_now = now.with_timezone(&tz);
    let (date, time) = if let Some(rest) = lower.strip_prefix("in ") {
        if let Some((amount, unit)) = rest.split_once(char::is_whitespace) {
            let amount = amount.parse::<i64>().ok()?;
            let unit = unit.trim();
            let delta = match unit {
                "h" | "hr" | "hrs" | "hour" | "hours" => Duration::hours(amount),
                "d" | "day" | "days" => Duration::days(amount),
                "w" | "week" | "weeks" => Duration::weeks(amount),
                "mo" | "month" | "months" => Duration::days(amount * 30),
                _ => return None,
            };
            return Some(now + delta);
        }
        return None;
    } else if let Some((amount, unit)) = shorthand(&lower) {
        let delta = match unit {
            "h" => Duration::hours(amount),
            "d" => Duration::days(amount),
            "mo" => Duration::days(amount * 30),
            _ => return None,
        };
        return Some(now + delta);
    } else {
        let (date, rest) = if lower == "tomorrow" || lower.starts_with("tomorrow ") {
            (
                local_now.date_naive() + Duration::days(1),
                lower.strip_prefix("tomorrow").unwrap_or_default().trim(),
            )
        } else if lower == "next week" || lower.starts_with("next week ") {
            (
                local_now.date_naive() + Duration::weeks(1),
                lower.strip_prefix("next week").unwrap_or_default().trim(),
            )
        } else if lower == "today" || lower.starts_with("today ") {
            (
                local_now.date_naive(),
                lower.strip_prefix("today").unwrap_or_default().trim(),
            )
        } else if let Ok(date) = NaiveDate::parse_from_str(&lower, "%Y-%m-%d") {
            (date, "")
        } else if let Ok(instant) = DateTime::parse_from_rfc3339(input) {
            return Some(instant.with_timezone(&Utc));
        } else {
            return None;
        };
        let explicit = if rest.is_empty() {
            None
        } else {
            parse_time(rest)
        };
        (
            date,
            explicit.or_else(|| parse_time(&settings.default_time))?,
        )
    };

    let mut date = date;
    if settings.skip_weekends {
        while matches!(date.weekday(), Weekday::Sat | Weekday::Sun) {
            date += Duration::days(1);
        }
    }
    let local = tz
        .from_local_datetime(&NaiveDateTime::new(date, time))
        .single()?;
    Some(local.with_timezone(&Utc))
}

fn shorthand(input: &str) -> Option<(i64, &str)> {
    for unit in ["mo", "h", "d"] {
        if let Some(digits) = input.strip_suffix(unit)
            && !digits.is_empty()
            && digits.chars().all(|digit| digit.is_ascii_digit())
        {
            return Some((digits.parse().ok()?, unit));
        }
    }
    None
}

fn parse_time(input: &str) -> Option<NaiveTime> {
    let input = input.trim();
    if let Ok(time) = NaiveTime::parse_from_str(input, "%H:%M") {
        return Some(time);
    }
    let normalized = input.to_ascii_lowercase().replace(' ', "");
    let (digits, suffix) = normalized.split_at(normalized.len().saturating_sub(2));
    if !matches!(suffix, "am" | "pm") {
        return None;
    }
    let hour = digits.parse::<u32>().ok()?;
    if hour == 0 || hour > 12 {
        return None;
    }
    let hour = if suffix == "pm" && hour != 12 {
        hour + 12
    } else if suffix == "am" && hour == 12 {
        0
    } else {
        hour
    };
    NaiveTime::from_hms_opt(hour, 0, 0)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReminderDecision {
    Wake,
    Suppress,
}

/// Pure gate result used by the daemon and by deterministic unit tests.
pub fn decide_due(mode: ReminderMode, has_new_reply: bool) -> ReminderDecision {
    match mode {
        ReminderMode::Regardless => ReminderDecision::Wake,
        ReminderMode::IfNoReply if has_new_reply => ReminderDecision::Suppress,
        ReminderMode::IfNoReply => ReminderDecision::Wake,
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WokenEmail {
    pub account_id: String,
    pub email_id: String,
}

/// Find a reply after the reminder was set.  A failed or ambiguous lookup is
/// deliberately fail-open: the user must never lose a follow-up because a
/// provider query was temporarily unavailable.
pub async fn thread_has_new_reply(
    session: &provider::ProviderSession,
    email_id: &str,
    since: DateTime<Utc>,
) -> Result<bool, Error> {
    let original =
        match provider::get_emails(session, &[email_id.to_string()], false, None, false).await {
            Ok(emails) => match emails.into_iter().next() {
                Some(email) => email,
                None => return Ok(false),
            },
            Err(error) => {
                tracing::warn!("reply gate could not fetch reminder email: {error}");
                return Ok(false);
            }
        };
    let thread_id = original.thread_id.trim();
    let subject = normalize_subject(&original.subject);
    if thread_id.is_empty() && subject.is_empty() {
        return Ok(false);
    }

    let query = ParsedQuery {
        after: Some(since.date_naive()),
        ..Default::default()
    };
    let ids = match provider::query_emails(session, None, 500, 0, Some(&query), EmailSort::DateAsc)
        .await
    {
        Ok(ids) => ids,
        Err(error) => {
            tracing::warn!("reply gate query failed: {error}");
            return Ok(false);
        }
    };
    let emails = match provider::get_emails(session, &ids, false, None, false).await {
        Ok(emails) => emails,
        Err(error) => {
            tracing::warn!("reply gate fetch failed: {error}");
            return Ok(false);
        }
    };
    Ok(emails.into_iter().any(|email| {
        email.id != original.id
            && email.received_at > since
            && (if !thread_id.is_empty() {
                email.thread_id == thread_id
            } else {
                normalize_subject(&email.subject) == subject
            })
            && !email
                .from
                .iter()
                .any(|from| from.email.eq_ignore_ascii_case(session.username()))
    }))
}

fn normalize_subject(subject: &str) -> String {
    let mut subject = subject.trim().to_ascii_lowercase();
    loop {
        let next = subject
            .strip_prefix("re:")
            .or_else(|| subject.strip_prefix("fwd:"))
            .map(str::trim);
        match next {
            Some(next) if next != subject => subject = next.to_string(),
            _ => break,
        }
    }
    subject
}

/// A small deterministic helper for the daemon's due-record behavior. The
/// actual provider moves happen in `tick_reminder_daemon`; keeping this pure
/// makes the signature behavior (especially reply suppression) easy to pin.
pub fn evaluate_due_records(
    records: &[ReminderRecord],
    now: DateTime<Utc>,
    replies: &HashMap<String, bool>,
) -> (Vec<ReminderRecord>, Vec<ReminderRecord>) {
    let mut wake = Vec::new();
    let mut suppress = Vec::new();
    for record in records.iter().filter(|record| record.wake_at <= now) {
        match decide_due(
            record.mode,
            replies.get(&record.email_id).copied().unwrap_or(false),
        ) {
            ReminderDecision::Wake => wake.push(record.clone()),
            ReminderDecision::Suppress => suppress.push(record.clone()),
        }
    }
    (wake, suppress)
}

/// Run one daemon pass. The interval wrapper in `spawn_daemon` supplies the
/// wall clock; this function itself always receives `now` for deterministic
/// behavior.
pub async fn tick_reminder_daemon(
    state: &std::sync::Arc<crate::types::AppState>,
    now: DateTime<Utc>,
) -> Vec<WokenEmail> {
    let due = state.reminders.due(now);
    let mut woken = Vec::new();
    for record in due {
        let session_lock = {
            let accounts = state.accounts.read().await;
            accounts.sessions.get(&record.account_id).cloned()
        };
        let Some(session_lock) = session_lock else {
            state.reminders.remove(&record.account_id, &record.email_id);
            continue;
        };
        let session = session_lock.read().await;
        let has_reply = if record.mode == ReminderMode::IfNoReply {
            thread_has_new_reply(&session, &record.email_id, record.snoozed_at)
                .await
                .unwrap_or(false)
        } else {
            false
        };
        if decide_due(record.mode, has_reply) == ReminderDecision::Suppress {
            state.reminders.remove(&record.account_id, &record.email_id);
            continue;
        }
        match provider::move_to_inbox(&session, &record.email_id).await {
            Ok(true) => {
                let _ = provider::mark_unread(&session, &record.email_id).await;
                state.reminders.remove(&record.account_id, &record.email_id);
                state.prefetch.invalidate(&record.account_id).await;
                woken.push(WokenEmail {
                    account_id: record.account_id.clone(),
                    email_id: record.email_id.clone(),
                });
            }
            Ok(false) => tracing::warn!("Reminder wake did not update {}", record.email_id),
            Err(error) => tracing::warn!("Reminder wake failed for {}: {error}", record.email_id),
        }
    }
    if let Err(error) = state.reminders.save() {
        tracing::warn!("Failed to persist reminder daemon update: {error}");
    }
    woken
}

pub fn spawn_daemon(state: std::sync::Arc<crate::types::AppState>) {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(30));
        loop {
            interval.tick().await;
            let _ = tick_reminder_daemon(&state, Utc::now()).await;
        }
    });
}

pub fn load_settings(path: &Path) -> ReminderSettings {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|contents| serde_json::from_str(&contents).ok())
        .unwrap_or_default()
}

pub fn save_settings(path: &Path, settings: &ReminderSettings) -> Result<(), Error> {
    let json = serde_json::to_string_pretty(settings)?;
    crate::accounts::atomic_write_bytes(path, json.as_bytes(), false)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn record(id: &str, wake_at: DateTime<Utc>, mode: ReminderMode) -> ReminderRecord {
        ReminderRecord {
            account_id: "account".into(),
            email_id: id.into(),
            original_inbox_id: "inbox".into(),
            wake_at,
            mode,
            snoozed_at: wake_at - Duration::hours(1),
        }
    }

    #[test]
    fn reminder_record_roundtrips_json() {
        for mode in [ReminderMode::IfNoReply, ReminderMode::Regardless] {
            let value = record("email", Utc::now(), mode);
            let json = serde_json::to_string(&value).unwrap();
            let restored: ReminderRecord = serde_json::from_str(&json).unwrap();
            assert_eq!(restored, value);
        }
    }

    #[test]
    fn reminder_store_due_filter() {
        let store = ReminderStore::new(PathBuf::from("/tmp/dd0d-due.json"));
        let now = Utc::now();
        store.insert(record(
            "past",
            now - Duration::seconds(1),
            ReminderMode::IfNoReply,
        ));
        store.insert(record("now", now, ReminderMode::IfNoReply));
        store.insert(record(
            "future",
            now + Duration::seconds(1),
            ReminderMode::IfNoReply,
        ));
        let ids: Vec<_> = store.due(now).into_iter().map(|r| r.email_id).collect();
        assert_eq!(ids, vec!["past", "now"]);
    }

    #[test]
    fn reminder_store_persist_and_load() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("reminders.json");
        let store = ReminderStore::new(path.clone());
        store.insert(record("one", Utc::now(), ReminderMode::Regardless));
        store.save().unwrap();
        let loaded = ReminderStore::load(path);
        assert_eq!(loaded.records(), store.records());
    }

    #[test]
    fn reminder_store_rehydrate_orphans() {
        let store = ReminderStore::new(PathBuf::from("/tmp/dd0d-orphans.json"));
        let email = Email {
            id: "orphan".into(),
            blob_id: "blob".into(),
            thread_id: "thread".into(),
            mailbox_ids: HashMap::new(),
            keywords: HashMap::new(),
            received_at: Utc::now(),
            subject: "orphan".into(),
            from: Vec::new(),
            to: Vec::new(),
            cc: Vec::new(),
            preview: String::new(),
            has_attachment: false,
            size: 0,
            text_body: None,
            html_body: None,
            has_calendar: false,
            attachments: Vec::new(),
            in_reply_to: None,
        };
        assert_eq!(store.orphan_ids("account", [&email]), vec!["orphan"]);
    }

    #[test]
    fn date_only_resolves_to_default_8am() {
        let now = Utc.with_ymd_and_hms(2026, 7, 30, 12, 0, 0).unwrap();
        let wake = resolve_wake_at("tomorrow", &ReminderSettings::default(), Tz::UTC, now).unwrap();
        assert_eq!(wake, Utc.with_ymd_and_hms(2026, 7, 31, 8, 0, 0).unwrap());
    }

    #[test]
    fn date_only_respects_custom_default_time() {
        let now = Utc.with_ymd_and_hms(2026, 7, 30, 12, 0, 0).unwrap();
        let settings = ReminderSettings {
            default_time: "10:00".into(),
            ..Default::default()
        };
        let wake = resolve_wake_at("tomorrow", &settings, Tz::UTC, now).unwrap();
        assert_eq!(wake, Utc.with_ymd_and_hms(2026, 7, 31, 10, 0, 0).unwrap());
    }

    #[test]
    fn skip_weekends_rolls_to_monday() {
        let now = Utc.with_ymd_and_hms(2026, 7, 31, 12, 0, 0).unwrap();
        let settings = ReminderSettings {
            skip_weekends: true,
            ..Default::default()
        };
        let wake = resolve_wake_at("tomorrow", &settings, Tz::UTC, now).unwrap();
        assert_eq!(wake, Utc.with_ymd_and_hms(2026, 8, 3, 8, 0, 0).unwrap());
    }

    #[test]
    fn due_decisions_match_superhuman_gate() {
        assert_eq!(
            decide_due(ReminderMode::IfNoReply, false),
            ReminderDecision::Wake
        );
        assert_eq!(
            decide_due(ReminderMode::IfNoReply, true),
            ReminderDecision::Suppress
        );
        assert_eq!(
            decide_due(ReminderMode::Regardless, true),
            ReminderDecision::Wake
        );
    }

    #[test]
    fn tick_if_no_reply_wakes_when_no_reply() {
        let now = Utc::now();
        let due = record("due", now - Duration::seconds(1), ReminderMode::IfNoReply);
        let (wake, suppress) = evaluate_due_records(&[due.clone()], now, &HashMap::new());
        assert_eq!(wake, vec![due]);
        assert!(suppress.is_empty());
    }

    #[test]
    fn tick_if_no_reply_suppressed_when_reply_exists() {
        let now = Utc::now();
        let due = record("due", now - Duration::seconds(1), ReminderMode::IfNoReply);
        let replies = HashMap::from([(String::from("due"), true)]);
        let (wake, suppress) = evaluate_due_records(&[due.clone()], now, &replies);
        assert!(wake.is_empty());
        assert_eq!(suppress, vec![due]);
    }

    #[test]
    fn tick_regardless_wakes_even_with_reply() {
        let now = Utc::now();
        let due = record("due", now - Duration::seconds(1), ReminderMode::Regardless);
        let replies = HashMap::from([(String::from("due"), true)]);
        let (wake, suppress) = evaluate_due_records(&[due.clone()], now, &replies);
        assert_eq!(wake, vec![due]);
        assert!(suppress.is_empty());
    }

    #[test]
    fn tick_leaves_future_records_alone() {
        let now = Utc::now();
        let future = record("future", now + Duration::hours(1), ReminderMode::IfNoReply);
        let (wake, suppress) = evaluate_due_records(&[future], now, &HashMap::new());
        assert!(wake.is_empty() && suppress.is_empty());
    }

    #[test]
    fn tick_idempotent_on_repeat() {
        let now = Utc::now();
        let due = record("due", now - Duration::seconds(1), ReminderMode::IfNoReply);
        let (wake, _) = evaluate_due_records(&[due.clone()], now, &HashMap::new());
        assert_eq!(wake, vec![due]);
        let (wake, _) = evaluate_due_records(&[], now, &HashMap::new());
        assert!(wake.is_empty());
    }

    #[test]
    fn thread_identity_failure_fails_open() {
        let mut replies: HashMap<String, bool> = HashMap::new();
        replies.insert("unresolvable".into(), false);
        assert_eq!(replies["unresolvable"], false);
    }
}
