//! Provider-agnostic request rate limiter.
//!
//! Three knobs per provider: concurrency cap (`Semaphore`), steady-state
//! spacing (`Spacer`), retry attempts. `execute()` runs a request closure
//! against those knobs, parses `Retry-After` on 429/503 responses, and
//! returns `Error::RateLimited` only after all retries are exhausted.
//!
//! The closure form (not `RequestBuilder`) is deliberate: streaming bodies
//! (RFC822 sends, blob uploads) can't be cloned, so retry must rebuild the
//! request from owned data each attempt.

use std::sync::Mutex;
use std::time::{Duration, Instant};
use tokio::sync::Semaphore;

use crate::error::Error;

/// Upper bound on a single backoff sleep, regardless of `Retry-After`
/// value or computed exponential. Prevents a server header of
/// "Retry-After: 86400" from hanging the client for a day.
///
/// 30s was chosen because servers requesting longer almost always
/// indicate a deeper outage worth surfacing to the user (as the final
/// `Error::RateLimited` after attempts exhaust) rather than silently
/// absorbing into a multi-minute hang.
const BACKOFF_CAP: Duration = Duration::from_secs(30);

/// Enforces a minimum interval between consecutive `acquire()` calls
/// across concurrent callers.
///
/// The fix that matters: `next_available = max(now, last + interval)` is
/// computed *while holding the lock*, and `last` is updated *before*
/// dropping the lock. Without this, concurrent callers can read the same
/// `last`, compute the same wake-up time, and all sleep until then —
/// producing exactly the burst the spacer exists to prevent.
///
/// Uses `std::sync::Mutex` (not `tokio::sync::Mutex`): the critical section
/// is infallible arithmetic and the lock is never held across an await.
/// `std::sync::Mutex` is faster and communicates the invariant. Poisoning
/// is impossible because the critical section can't panic.
pub struct Spacer {
    last: Mutex<Instant>,
    interval: Duration,
}

impl Spacer {
    pub fn new(interval: Duration) -> Self {
        // Initialize `last` in the past so the first acquire is immediate.
        let seed = Instant::now()
            .checked_sub(interval)
            .unwrap_or_else(Instant::now);
        Self {
            last: Mutex::new(seed),
            interval,
        }
    }

    pub async fn acquire(&self) {
        let wake_at = {
            // Poisoning is impossible: critical section is infallible
            // arithmetic (checked_add saturates instead of panicking).
            // `.unwrap()` would only fire on memory corruption.
            let mut last = self.last.lock().unwrap();
            let now = Instant::now();
            // `Instant + Duration` panics on overflow; `checked_add` saturates
            // by falling back to `now` (i.e., "fire immediately"). With
            // intervals of 80-125ms this is theoretical, but it keeps the
            // "infallible arithmetic" invariant literally true.
            let next = last.checked_add(self.interval).unwrap_or(now);
            let wake = std::cmp::max(now, next);
            *last = wake;
            wake
        };
        let now = Instant::now();
        if wake_at > now {
            tokio::time::sleep(wake_at - now).await;
        }
    }
}

/// Reserved permits for user-blocking requests. Two is enough for the
/// interactive shapes we have (an email open is one `messages.get`, an RSVP
/// or unsubscribe is one or two) while staying small enough that a bulk
/// user action can't turn the lane into a second unbounded pool.
///
/// The lane is additive: peak in-flight requests can briefly reach
/// `concurrency + PRIORITY_PERMITS` (Gmail 5 → 7). The shared spacer keeps
/// the request *rate* unchanged; only providers that throttle on raw
/// concurrent connections would notice, and both Gmail and Graph throttle
/// on rate/quota.
const PRIORITY_PERMITS: usize = 2;

pub struct RateLimiter {
    sem: Semaphore,
    /// Separate small pool for interactive requests. `Semaphore` wakes
    /// waiters FIFO, so during a warm pass the main pool's queue holds
    /// hundreds of background fetches — a user opening an email used to
    /// wait out that entire queue (measured: tens of seconds to minutes).
    /// The shared `spacer` still paces every request start, so the total
    /// request rate is unchanged; only queue position differs.
    priority_sem: Semaphore,
    spacer: Spacer,
    max_attempts: u32,
    name: &'static str,
    concurrency: usize,
    spacing: Duration,
}

impl RateLimiter {
    pub fn new(
        name: &'static str,
        concurrency: usize,
        spacing: Duration,
        max_attempts: u32,
    ) -> Self {
        Self {
            sem: Semaphore::new(concurrency),
            priority_sem: Semaphore::new(PRIORITY_PERMITS),
            spacer: Spacer::new(spacing),
            max_attempts,
            name,
            concurrency,
            spacing,
        }
    }

    pub fn name(&self) -> &'static str {
        self.name
    }
    pub fn concurrency(&self) -> usize {
        self.concurrency
    }
    pub fn spacing(&self) -> Duration {
        self.spacing
    }

    /// Run `make_req` under the limiter, retrying on 429/503 with
    /// `Retry-After` honored and exponential backoff with jitter as
    /// fallback. Returns `Err(Error::RateLimited)` only after all
    /// `max_attempts` attempts hit a rate-limit status.
    ///
    /// Non-rate-limit non-2xx responses are returned as `Ok(response)` —
    /// the caller's classify_* function reads the body and maps it.
    pub async fn execute<F, Fut>(&self, op: &str, make_req: F) -> Result<reqwest::Response, Error>
    where
        F: FnMut() -> Fut,
        Fut: std::future::Future<Output = reqwest::Result<reqwest::Response>>,
    {
        self.execute_prioritized(false, op, make_req).await
    }

    /// Like [`execute`](Self::execute), but `priority: true` acquires from
    /// the small reserved pool instead of queuing behind background work.
    /// Reserve it for requests a user is actively waiting on (opening an
    /// email, an RSVP) — bulk fan-outs must stay on the main pool or they
    /// would starve the lane they're meant to be yielding to.
    pub async fn execute_prioritized<F, Fut>(
        &self,
        priority: bool,
        op: &str,
        mut make_req: F,
    ) -> Result<reqwest::Response, Error>
    where
        F: FnMut() -> Fut,
        Fut: std::future::Future<Output = reqwest::Result<reqwest::Response>>,
    {
        let sem = if priority {
            &self.priority_sem
        } else {
            &self.sem
        };
        let mut last_retry_after: Option<Duration> = None;

        for attempt in 0..self.max_attempts {
            let permit = sem
                .acquire()
                .await
                .map_err(|e| Error::Internal(format!("{} limiter sem closed: {e}", self.name)))?;
            self.spacer.acquire().await;

            let resp = match make_req().await {
                Ok(r) => r,
                Err(e) => {
                    drop(permit);
                    // Transient network failure — back off and retry.
                    if attempt + 1 < self.max_attempts && (e.is_timeout() || e.is_connect()) {
                        let backoff = backoff_with_jitter(attempt);
                        tracing::warn!(
                            provider = self.name,
                            op = op,
                            attempt,
                            backoff_ms = backoff.as_millis() as u64,
                            error = %e,
                            "transient request error — backing off"
                        );
                        tokio::time::sleep(backoff).await;
                        continue;
                    }
                    return Err(Error::Network(format!("{} {op}: {e}", self.name)));
                }
            };

            let status = resp.status();

            if status.is_success() {
                tracing::debug!(
                    provider = self.name,
                    op = op,
                    attempt,
                    status = status.as_u16(),
                    "request ok"
                );
                return Ok(resp);
            }

            let is_throttle = status == reqwest::StatusCode::TOO_MANY_REQUESTS
                || status == reqwest::StatusCode::SERVICE_UNAVAILABLE;

            if is_throttle {
                let retry_after = parse_retry_after(resp.headers());
                last_retry_after = retry_after.or(last_retry_after);
                // Release the permit BEFORE the backoff sleep so other
                // tasks can drain. Holding it collapses effective
                // concurrency exactly when we want backpressure spread.
                drop(permit);

                if attempt + 1 >= self.max_attempts {
                    tracing::warn!(
                        provider = self.name,
                        op = op,
                        attempt,
                        status = status.as_u16(),
                        retry_after_secs = retry_after.map(|d| d.as_secs()),
                        "rate-limit retries exhausted"
                    );
                    break;
                }

                let backoff = retry_after
                    .map(|d| std::cmp::min(d, BACKOFF_CAP))
                    .unwrap_or_else(|| backoff_with_jitter(attempt));
                tracing::warn!(
                    provider = self.name,
                    op = op,
                    attempt,
                    status = status.as_u16(),
                    retry_after_secs = retry_after.map(|d| d.as_secs()),
                    backoff_ms = backoff.as_millis() as u64,
                    "rate limited — backing off"
                );
                tokio::time::sleep(backoff).await;
                continue;
            }

            // Non-2xx, non-throttle: hand the response back to the caller's
            // classify_* function. The limiter doesn't try to interpret
            // application-level errors.
            return Ok(resp);
        }

        Err(Error::RateLimited {
            retry_after: last_retry_after,
        })
    }
}

/// Exponential backoff with equal-jitter: `wait = base/2 + random(0, base/2)`.
/// Capped at `BACKOFF_CAP`. Equal jitter (not full jitter) guarantees a
/// minimum wait, which matters when the server is genuinely overloaded.
///
/// `checked_shl` saturates `1 << attempt` at `u64::MAX` for arbitrarily large
/// attempt values; the load-bearing upper bound is `BACKOFF_CAP`, not the
/// shift width.
fn backoff_with_jitter(attempt: u32) -> Duration {
    let base_secs = 1u64.checked_shl(attempt).unwrap_or(u64::MAX);
    let cap = std::cmp::min(Duration::from_secs(base_secs), BACKOFF_CAP);
    let half = cap.as_secs_f64() / 2.0;
    let jitter = rand::random::<f64>() * half;
    Duration::from_secs_f64(half + jitter)
}

/// Parse RFC 7231 `Retry-After`: delta-seconds (e.g. `120`) or HTTP-date
/// (RFC 2822 / IMF-fixdate). Returns `None` for malformed values; the
/// caller falls back to its own backoff.
fn parse_retry_after(headers: &reqwest::header::HeaderMap) -> Option<Duration> {
    let raw = headers.get(reqwest::header::RETRY_AFTER)?.to_str().ok()?;
    let raw = raw.trim();
    if let Ok(secs) = raw.parse::<u64>() {
        return Some(Duration::from_secs(secs));
    }
    if let Ok(dt) = chrono::DateTime::parse_from_rfc2822(raw) {
        let now = chrono::Utc::now();
        let diff = dt.with_timezone(&chrono::Utc) - now;
        let secs = diff.num_seconds();
        if secs > 0 {
            return Some(Duration::from_secs(secs as u64));
        }
        // Past HTTP-date: most often clock skew, not "retry now". Returning
        // `Some(ZERO)` would short-circuit `execute`'s backoff and hammer
        // a server that's likely already overloaded. Fall through to None
        // so the jittered exponential fallback takes over.
        return None;
    }
    None
}

/// Gmail surfaces quota errors in the response body as JSON `reason`
/// strings, sometimes under a 403 rather than 429 — particularly the
/// per-user-per-100-seconds quota. Match the documented reason codes.
pub fn is_gmail_rate_limit_body(body: &str) -> bool {
    body.contains("userRateLimitExceeded")
        || body.contains("rateLimitExceeded")
        || body.contains("RESOURCE_EXHAUSTED")
}

/// JMAP returns `urn:ietf:params:jmap:error:limit` inside individual
/// method responses (HTTP can still be 200). Scan the methodResponses
/// array for that error type.
pub fn is_jmap_rate_limit_response(resp: &serde_json::Value) -> bool {
    let Some(arr) = resp.get("methodResponses").and_then(|v| v.as_array()) else {
        return false;
    };
    arr.iter().any(|mr| {
        mr.get(1)
            .and_then(|x| x.get("type"))
            .and_then(|t| t.as_str())
            == Some("urn:ietf:params:jmap:error:limit")
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    // ---- Spacer ----

    #[tokio::test]
    async fn spacer_first_call_is_immediate() {
        let spacer = Spacer::new(Duration::from_millis(100));
        let start = Instant::now();
        spacer.acquire().await;
        // Tolerance is intentionally well below `interval` (100ms) so a
        // regression that makes the first call sleep for `interval` would
        // still trip the assertion, while busy CI runners get headroom.
        assert!(start.elapsed() < Duration::from_millis(50));
    }

    #[tokio::test]
    async fn spacer_enforces_interval_under_contention() {
        // 10 concurrent acquires on a 25ms spacer should take at least
        // 9 * 25ms = 225ms of wall-clock (first is immediate, then each
        // sequenced after the prior).
        let spacer = Arc::new(Spacer::new(Duration::from_millis(25)));
        let n = 10;
        let start = Instant::now();
        let mut handles = Vec::with_capacity(n);
        for _ in 0..n {
            let s = spacer.clone();
            handles.push(tokio::spawn(async move {
                s.acquire().await;
                Instant::now()
            }));
        }
        let mut times = Vec::with_capacity(n);
        for h in handles {
            times.push(h.await.unwrap());
        }
        let total = start.elapsed();
        assert!(
            total >= Duration::from_millis(9 * 25),
            "expected >= 225ms, got {:?}",
            total
        );
        // No thundering herd: pairwise diffs should be >= ~interval.
        times.sort();
        for w in times.windows(2) {
            let diff = w[1] - w[0];
            assert!(
                diff >= Duration::from_millis(20),
                "pairwise gap {:?} too small — thundering herd",
                diff
            );
        }
    }

    // ---- Retry-After parsing ----

    #[test]
    fn parse_retry_after_delta_seconds() {
        let mut h = reqwest::header::HeaderMap::new();
        h.insert(reqwest::header::RETRY_AFTER, "30".parse().unwrap());
        assert_eq!(parse_retry_after(&h), Some(Duration::from_secs(30)));
    }

    #[test]
    fn parse_retry_after_http_date() {
        let future = chrono::Utc::now() + chrono::Duration::seconds(120);
        let mut h = reqwest::header::HeaderMap::new();
        let raw = future.to_rfc2822();
        h.insert(reqwest::header::RETRY_AFTER, raw.parse().unwrap());
        let parsed = parse_retry_after(&h).expect("parsed");
        assert!(
            parsed.as_secs() >= 110 && parsed.as_secs() <= 130,
            "got {parsed:?}"
        );
    }

    #[test]
    fn parse_retry_after_missing_returns_none() {
        let h = reqwest::header::HeaderMap::new();
        assert_eq!(parse_retry_after(&h), None);
    }

    #[test]
    fn parse_retry_after_garbage_returns_none() {
        let mut h = reqwest::header::HeaderMap::new();
        h.insert(reqwest::header::RETRY_AFTER, "nope".parse().unwrap());
        assert_eq!(parse_retry_after(&h), None);
    }

    #[test]
    fn parse_retry_after_past_http_date_returns_none() {
        // A past HTTP-date almost always indicates clock skew. Returning
        // None lets `execute()` fall through to jittered exponential
        // backoff instead of issuing an immediate herd retry.
        let past = chrono::Utc::now() - chrono::Duration::seconds(60);
        let mut h = reqwest::header::HeaderMap::new();
        h.insert(
            reqwest::header::RETRY_AFTER,
            past.to_rfc2822().parse().unwrap(),
        );
        assert_eq!(parse_retry_after(&h), None);
    }

    // ---- Backoff bounds ----

    #[test]
    fn backoff_with_jitter_handles_arbitrary_attempt_without_panic() {
        // Two properties at the boundaries:
        //   1. No shift-overflow panic at any `attempt`, including u32::MAX
        //      (validates the `checked_shl` saturation path).
        //   2. The equal-jitter floor `d >= cap/2` holds even at saturation,
        //      where `cap = min(2^attempt, BACKOFF_CAP)`. A regression that
        //      flattens the jitter formula (e.g., dropping the `half +`
        //      term) would otherwise go undetected at very high attempts.
        for n in [0u32, 5, u32::MAX] {
            let d = backoff_with_jitter(n);
            let base_secs = 1u64.checked_shl(n).unwrap_or(u64::MAX);
            let cap = std::cmp::min(Duration::from_secs(base_secs), BACKOFF_CAP);
            assert!(d <= cap, "attempt {n}: {d:?} > cap={cap:?}");
            assert!(
                d >= cap / 2,
                "attempt {n}: {d:?} below equal-jitter floor {:?}",
                cap / 2
            );
        }
    }

    #[test]
    fn backoff_with_jitter_respects_bounds() {
        for attempt in 0..6 {
            let base_secs = 1u64.checked_shl(attempt).unwrap_or(u64::MAX);
            let cap = std::cmp::min(Duration::from_secs(base_secs), BACKOFF_CAP);
            for _ in 0..100 {
                let d = backoff_with_jitter(attempt);
                assert!(d >= cap / 2, "attempt {attempt}: {d:?} below half-cap");
                assert!(d <= cap, "attempt {attempt}: {d:?} above cap");
            }
        }
    }

    // ---- Body detectors ----

    #[test]
    fn gmail_body_detector_matches_known_strings() {
        assert!(is_gmail_rate_limit_body(
            r#"{"error":{"errors":[{"reason":"userRateLimitExceeded"}]}}"#
        ));
        assert!(is_gmail_rate_limit_body(
            r#"{"error":{"errors":[{"reason":"rateLimitExceeded"}]}}"#
        ));
        assert!(is_gmail_rate_limit_body(
            r#"{"status":"RESOURCE_EXHAUSTED"}"#
        ));
        assert!(!is_gmail_rate_limit_body(
            r#"{"error":{"message":"unauthorized"}}"#
        ));
    }

    #[test]
    fn jmap_response_detector_matches_limit_error() {
        let resp: serde_json::Value = serde_json::json!({
            "methodResponses": [
                ["error", {"type": "urn:ietf:params:jmap:error:limit"}, "0"]
            ]
        });
        assert!(is_jmap_rate_limit_response(&resp));
    }

    #[test]
    fn jmap_response_detector_ignores_normal_responses() {
        let resp: serde_json::Value = serde_json::json!({
            "methodResponses": [
                ["Email/get", {"list": []}, "0"]
            ]
        });
        assert!(!is_jmap_rate_limit_response(&resp));
        assert!(!is_jmap_rate_limit_response(&serde_json::json!({})));
    }

    // ---- RateLimiter accessors ----

    #[test]
    fn limiter_exposes_configuration() {
        let lim = RateLimiter::new("test", 7, Duration::from_millis(50), 3);
        assert_eq!(lim.name(), "test");
        assert_eq!(lim.concurrency(), 7);
        assert_eq!(lim.spacing(), Duration::from_millis(50));
    }

    // ---- execute() against a local axum server ----

    use axum::Router;
    use axum::http::HeaderMap;
    use axum::http::StatusCode;
    use axum::response::IntoResponse;
    use axum::routing::get;
    use std::sync::atomic::{AtomicU32, Ordering};

    async fn spawn_server<F>(make_app: F) -> String
    where
        F: FnOnce() -> Router,
    {
        let app = make_app();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        format!("http://{addr}")
    }

    #[tokio::test]
    async fn execute_returns_2xx_immediately() {
        let base = spawn_server(|| Router::new().route("/ok", get(|| async { "ok" }))).await;
        let client = reqwest::Client::new();
        let lim = RateLimiter::new("t", 4, Duration::from_millis(1), 3);
        let resp = lim
            .execute("ok", || async {
                client.get(format!("{base}/ok")).send().await
            })
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
    }

    #[tokio::test]
    async fn priority_lane_bypasses_saturated_main_pool() {
        // A 429-then-200 endpoint: the priority call must survive one
        // retry without falling back to the main pool.
        let hits = std::sync::Arc::new(AtomicU32::new(0));
        let hits_for_handler = hits.clone();
        let base = spawn_server(move || {
            Router::new().route(
                "/flaky",
                get(move || {
                    let hits = hits_for_handler.clone();
                    async move {
                        if hits.fetch_add(1, Ordering::SeqCst) == 0 {
                            (
                                axum::http::StatusCode::TOO_MANY_REQUESTS,
                                [(axum::http::header::RETRY_AFTER, "0")],
                                "slow down",
                            )
                                .into_response()
                        } else {
                            "ok".into_response()
                        }
                    }
                }),
            )
        })
        .await;
        let client = reqwest::Client::new();
        let lim = RateLimiter::new("t", 3, Duration::from_millis(1), 3);

        // Saturate the main pool the way a warm pass does: every permit
        // held, so a normal `execute` queues indefinitely.
        let _held = lim.sem.acquire_many(3).await.unwrap();

        // Normal lane: must still be stuck behind the held permits.
        let blocked = tokio::time::timeout(
            Duration::from_millis(200),
            lim.execute("blocked", || async {
                client.get(format!("{base}/flaky")).send().await
            }),
        )
        .await;
        assert!(
            blocked.is_err(),
            "a non-priority call must queue behind the saturated main pool"
        );

        // Priority lane: completes despite the saturated main pool, and
        // its 429 retry re-acquires the priority semaphore (a fallback to
        // the main pool would hang here and trip the timeout).
        let resp = tokio::time::timeout(
            Duration::from_secs(5),
            lim.execute_prioritized(true, "priority", || async {
                client.get(format!("{base}/flaky")).send().await
            }),
        )
        .await
        .expect("priority call must not queue behind the main pool")
        .unwrap();
        assert_eq!(resp.status(), 200);
        assert!(
            hits.load(Ordering::SeqCst) >= 2,
            "the 429 must have been retried on the priority lane"
        );
    }

    #[tokio::test]
    async fn execute_honors_retry_after_seconds() {
        let hits = std::sync::Arc::new(AtomicU32::new(0));
        let hits_for_handler = hits.clone();
        let base = spawn_server(move || {
            Router::new().route(
                "/throttle",
                get(move || {
                    let hits = hits_for_handler.clone();
                    async move {
                        let n = hits.fetch_add(1, Ordering::SeqCst);
                        if n == 0 {
                            let mut h = HeaderMap::new();
                            h.insert("retry-after", "1".parse().unwrap());
                            (StatusCode::TOO_MANY_REQUESTS, h, "throttled").into_response()
                        } else {
                            (StatusCode::OK, "now-ok").into_response()
                        }
                    }
                }),
            )
        })
        .await;
        let client = reqwest::Client::new();
        let lim = RateLimiter::new("t", 2, Duration::from_millis(1), 3);
        let start = Instant::now();
        let resp = lim
            .execute("throttle", || async {
                client.get(format!("{base}/throttle")).send().await
            })
            .await
            .unwrap();
        let elapsed = start.elapsed();
        assert_eq!(resp.status(), 200);
        assert!(
            elapsed >= Duration::from_millis(900),
            "expected ~1s wait, got {elapsed:?}"
        );
        assert_eq!(hits.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn execute_gives_up_after_max_attempts_with_rate_limited_error() {
        let base = spawn_server(|| {
            Router::new().route(
                "/always429",
                get(|| async { (StatusCode::TOO_MANY_REQUESTS, "no").into_response() }),
            )
        })
        .await;
        let client = reqwest::Client::new();
        let lim = RateLimiter::new("t", 2, Duration::from_millis(1), 2);
        let result = lim
            .execute("always429", || async {
                client.get(format!("{base}/always429")).send().await
            })
            .await;
        assert!(
            matches!(result, Err(Error::RateLimited { .. })),
            "expected RateLimited, got {result:?}"
        );
    }

    #[tokio::test]
    async fn execute_surfaces_most_recent_retry_after_after_exhaustion() {
        // Three 429s with Retry-After: 1, then 7, then none.
        // The server's most-recent Retry-After signal should win — not the
        // first one seen. Mutation-tested: flipping the production code to
        // `last_retry_after.or(retry_after)` (first-wins) causes this test
        // to fail with Some(1), proving it pins the policy.
        let hits = std::sync::Arc::new(AtomicU32::new(0));
        let base = spawn_server(move || {
            Router::new().route(
                "/escalating",
                get(move || {
                    let hits = hits.clone();
                    async move {
                        let n = hits.fetch_add(1, Ordering::SeqCst);
                        let mut h = HeaderMap::new();
                        match n {
                            0 => {
                                h.insert("retry-after", "1".parse().unwrap());
                            }
                            1 => {
                                h.insert("retry-after", "7".parse().unwrap());
                            }
                            _ => {}
                        }
                        (StatusCode::TOO_MANY_REQUESTS, h, "throttled").into_response()
                    }
                }),
            )
        })
        .await;
        let client = reqwest::Client::new();
        let lim = RateLimiter::new("t", 2, Duration::from_millis(1), 3);
        let err = lim
            .execute("escalating", || async {
                client.get(format!("{base}/escalating")).send().await
            })
            .await
            .expect_err("expected all three attempts to be throttled");
        match err {
            Error::RateLimited {
                retry_after: Some(d),
            } => {
                assert_eq!(
                    d.as_secs(),
                    7,
                    "expected the most-recent Retry-After (7s) to win"
                );
            }
            other => panic!("expected RateLimited{{retry_after: Some(7)}}, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn execute_returns_non_throttle_4xx_to_caller() {
        let base = spawn_server(|| {
            Router::new().route(
                "/bad",
                get(|| async { (StatusCode::BAD_REQUEST, "nope").into_response() }),
            )
        })
        .await;
        let client = reqwest::Client::new();
        let lim = RateLimiter::new("t", 2, Duration::from_millis(1), 3);
        let resp = lim
            .execute("bad", || async {
                client.get(format!("{base}/bad")).send().await
            })
            .await
            .unwrap();
        assert_eq!(resp.status(), 400);
    }

    #[tokio::test]
    async fn execute_releases_permit_during_backoff() {
        // Two endpoints share a limiter with concurrency=2.
        // One endpoint always 429s with Retry-After: 1; the other 200s.
        // Spawn two tasks hitting the 429 endpoint, then one task hitting
        // the OK endpoint. The OK task must complete during the 429
        // backoff window — otherwise the permits were held across sleep.
        let base = spawn_server(|| {
            Router::new()
                .route(
                    "/throttle",
                    get(|| async {
                        let mut h = HeaderMap::new();
                        h.insert("retry-after", "2".parse().unwrap());
                        (StatusCode::TOO_MANY_REQUESTS, h, "no").into_response()
                    }),
                )
                .route("/ok", get(|| async { "fine" }))
        })
        .await;
        let client = reqwest::Client::new();
        let lim = Arc::new(RateLimiter::new("t", 2, Duration::from_millis(1), 2));

        let base_t = base.clone();
        let lim_t = lim.clone();
        let client_t = client.clone();
        let t1 = tokio::spawn(async move {
            lim_t
                .execute("throttle", || async {
                    client_t.get(format!("{base_t}/throttle")).send().await
                })
                .await
                .ok();
        });
        let base_t2 = base.clone();
        let lim_t2 = lim.clone();
        let client_t2 = client.clone();
        let t2 = tokio::spawn(async move {
            lim_t2
                .execute("throttle", || async {
                    client_t2.get(format!("{base_t2}/throttle")).send().await
                })
                .await
                .ok();
        });

        // Give the throttle tasks a head start so they're parked in backoff.
        tokio::time::sleep(Duration::from_millis(200)).await;

        let start = Instant::now();
        let resp = lim
            .execute("ok", || async {
                client.get(format!("{base}/ok")).send().await
            })
            .await
            .unwrap();
        let elapsed = start.elapsed();
        assert_eq!(resp.status(), 200);
        assert!(
            elapsed < Duration::from_millis(500),
            "ok task waited {elapsed:?} — permits likely held during backoff"
        );
        let _ = tokio::join!(t1, t2);
    }
}
