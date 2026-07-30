use axum::http::StatusCode;
use axum::http::header::{HeaderName, HeaderValue};
use axum::response::{IntoResponse, Response};
use std::fmt;
use std::time::Duration;

#[derive(Debug)]
pub enum Error {
    Auth(String),
    Network(String),
    NotConnected,
    NotFound(String),
    BadRequest(String),
    Conflict(String),
    Internal(String),
    RateLimited {
        retry_after: Option<Duration>,
    },
    /// A CalDAV calendar operation was attempted against a Fastmail account
    /// that has no `app-password` configured.
    ///
    /// Fastmail's CalDAV endpoint rejects the JMAP/MCP-only API token (Bearer)
    /// with "Not a valid protocol for this access token." CalDAV requires a
    /// separate app password sent as HTTP Basic auth. A missing app password
    /// is a config-state problem the user can act on (add an App password in
    /// Settings), not a transient network blip — so the CalDAV functions
    /// return this *before* issuing any HTTP request, and the caller surfaces
    /// it to the UI instead of `warn!`-ing past an `Ok(false)`.
    CalendarAuthUnconfigured,
    /// CalDAV calendar-home discovery failed (kata wybm).
    ///
    /// The four CalDAV functions no longer hardcode `/Default/` (which
    /// 301→404's on real Fastmail accounts); they discover the user's default
    /// writable calendar collection via PROPFIND and cache it. If that
    /// discovery can't resolve a writable calendar — the PROPFIND chain
    /// returns non-207, the server exposes no `current-user-principal`, or no
    /// writable calendar collection exists — this is surfaced instead of
    /// silently writing to a wrong/missing collection. The `String` is an
    /// operator-facing detail (which step failed) logged at WARN and used in
    /// the account-error banner rationale; it is *not* sent to the HTTP
    /// client (`IntoResponse` substitutes `CALENDAR_DISCOVERY_FAILED_MSG` so
    /// no URL/username leaks).
    CalendarDiscoveryFailed(String),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Auth(msg) => write!(f, "authentication failed: {msg}"),
            Error::Network(msg) => write!(f, "network error: {msg}"),
            Error::NotConnected => write!(f, "not connected to email server"),
            Error::NotFound(msg) => write!(f, "not found: {msg}"),
            Error::BadRequest(msg) => write!(f, "bad request: {msg}"),
            Error::Conflict(msg) => write!(f, "conflict: {msg}"),
            Error::CalendarAuthUnconfigured => write!(
                f,
                "calendar auth unconfigured: Fastmail app password not set"
            ),
            Error::CalendarDiscoveryFailed(msg) => {
                write!(f, "calendar discovery failed: {msg}")
            }
            Error::Internal(msg) => write!(f, "internal error: {msg}"),
            Error::RateLimited { retry_after } => match retry_after {
                Some(d) => write!(f, "rate limited — retry after {}s", d.as_secs()),
                None => write!(f, "rate limited"),
            },
        }
    }
}

/// The actionable client-facing message for `Error::CalendarAuthUnconfigured`.
/// Used both by `IntoResponse` (the 400 body for the explicit RSVP / add-to-
/// calendar routes) and by the fire-and-forget spawned calendar writers in
/// `get_email` (which can't return an HTTP response, so they push this to the
/// account-error banner instead). One constant so the banner and the 400
/// body can't drift.
pub const CALENDAR_AUTH_UNCONFIGURED_MSG: &str =
    "Fastmail calendar sync needs an app password — add one in Settings";

/// The actionable client-facing message for `Error::CalendarDiscoveryFailed`.
/// Used by `IntoResponse` (the 503 body) and by the fire-and-forget spawned
/// calendar writers in `get_email` (which push it to the account-error
/// banner). One constant so the banner and the 503 body can't drift, and so
/// the discovery-specific detail inside the error (URL/username) never
/// reaches the client — only this generic, actionable message does. Paired
/// with `CALENDAR_AUTH_UNCONFIGURED_MSG` (the m5yp credential case); this is
/// the wybm discovery case.
pub const CALENDAR_DISCOVERY_FAILED_MSG: &str = "Couldn't find your Fastmail calendar to sync the event — check your calendars in Fastmail Settings";

impl std::error::Error for Error {}

impl From<reqwest::Error> for Error {
    fn from(e: reqwest::Error) -> Self {
        Error::Network(e.to_string())
    }
}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Error::Internal(e.to_string())
    }
}

impl From<serde_json::Error> for Error {
    fn from(e: serde_json::Error) -> Self {
        Error::Internal(e.to_string())
    }
}

impl IntoResponse for Error {
    fn into_response(self) -> Response {
        let mut retry_after_header: Option<HeaderValue> = None;
        let (status, client_message) = match &self {
            Error::Auth(_) => (StatusCode::UNAUTHORIZED, "authentication failed".into()),
            Error::NotFound(msg) => (StatusCode::NOT_FOUND, format!("not found: {msg}")),
            Error::BadRequest(msg) => (StatusCode::BAD_REQUEST, format!("bad request: {msg}")),
            Error::Conflict(msg) => (StatusCode::CONFLICT, format!("conflict: {msg}")),
            // Config-state, not auth: the credential is absent rather than
            // rejected. 400 (not 401) so the UI treats it as a fixable setup
            // step, and the message names the field to add so the banner is
            // actionable instead of opaque.
            Error::CalendarAuthUnconfigured => (
                StatusCode::BAD_REQUEST,
                CALENDAR_AUTH_UNCONFIGURED_MSG.to_string(),
            ),
            // Discovery couldn't resolve a writable calendar — either the
            // PROPFIND chain broke (transient server/transport) or no
            // writable calendar collection exists (config). 503 surfaces it
            // as "try again / check setup" rather than a silent Ok. The
            // operator-facing detail (which step failed, which URL) is logged
            // at WARN for debugging; only the generic, actionable message is
            // sent to the client so no URL/username leaks.
            Error::CalendarDiscoveryFailed(msg) => {
                tracing::warn!("Calendar discovery failed: {msg}");
                (
                    StatusCode::SERVICE_UNAVAILABLE,
                    CALENDAR_DISCOVERY_FAILED_MSG.to_string(),
                )
            }
            Error::NotConnected => (
                StatusCode::SERVICE_UNAVAILABLE,
                "not connected to email server".into(),
            ),
            Error::Network(msg) => {
                tracing::warn!("Network error: {msg}");
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "network error".to_string(),
                )
            }
            Error::Internal(msg) => {
                tracing::warn!("Internal error: {msg}");
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "internal error".to_string(),
                )
            }
            Error::RateLimited { retry_after } => {
                if let Some(d) = retry_after {
                    retry_after_header = HeaderValue::from_str(&d.as_secs().to_string()).ok();
                }
                (StatusCode::TOO_MANY_REQUESTS, "rate limited".to_string())
            }
        };
        let body = serde_json::json!({ "error": client_message });
        let mut resp = (status, axum::Json(body)).into_response();
        if let Some(v) = retry_after_header {
            resp.headers_mut()
                .insert(HeaderName::from_static("retry-after"), v);
        }
        resp
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::to_bytes;
    use axum::response::IntoResponse;

    async fn response_status_and_body(error: Error) -> (StatusCode, String) {
        let resp = error.into_response();
        let status = resp.status();
        let bytes = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        (status, String::from_utf8_lossy(&bytes).to_string())
    }

    #[tokio::test]
    async fn auth_error_returns_401() {
        let (status, _) = response_status_and_body(Error::Auth("bad token".into())).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn not_found_returns_404() {
        let (status, body) = response_status_and_body(Error::NotFound("email xyz".into())).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert!(body.contains("not found"));
    }

    #[tokio::test]
    async fn bad_request_returns_400() {
        let (status, _) = response_status_and_body(Error::BadRequest("missing field".into())).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn not_connected_returns_503() {
        let (status, _) = response_status_and_body(Error::NotConnected).await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    }

    #[tokio::test]
    async fn network_error_returns_500() {
        let (status, _) =
            response_status_and_body(Error::Network("connection refused".into())).await;
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    }

    #[tokio::test]
    async fn internal_error_returns_500() {
        let (status, _) =
            response_status_and_body(Error::Internal("database corruption".into())).await;
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    }

    #[tokio::test]
    async fn internal_error_does_not_leak_details() {
        let (_, body) =
            response_status_and_body(Error::Internal("secret db password exposed".into())).await;
        assert!(!body.contains("secret db password exposed"));
        assert!(body.contains("internal error"));
    }

    #[tokio::test]
    async fn network_error_does_not_leak_details() {
        let (_, body) =
            response_status_and_body(Error::Network("10.0.0.5:5432 refused".into())).await;
        assert!(!body.contains("10.0.0.5"));
        assert!(body.contains("network error"));
    }

    #[tokio::test]
    async fn auth_error_does_not_leak_token() {
        let (_, body) = response_status_and_body(Error::Auth("token fmu1-abc123xyz".into())).await;
        assert!(!body.contains("fmu1-abc123xyz"));
        assert!(body.contains("authentication failed"));
    }

    #[tokio::test]
    async fn calendar_auth_unconfigured_returns_400_with_actionable_message() {
        // Config-state, not auth: 400 (not 401) so the UI treats it as a
        // fixable setup step. The message must name the field to add so the
        // banner is actionable, and must not leak any secret.
        let (status, body) = response_status_and_body(Error::CalendarAuthUnconfigured).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(
            body.contains("app password"),
            "body must name the field: {body}"
        );
        assert!(
            body.contains("Settings"),
            "body must point at Settings: {body}"
        );
    }

    #[tokio::test]
    async fn calendar_discovery_failed_returns_503_and_does_not_leak_detail() {
        // The operator detail (which PROPFIND step failed, the URL/username)
        // stays in the WARN log; the HTTP client gets a generic, actionable
        // message so no account path leaks. 503 surfaces "try again / check
        // setup" rather than the silent Ok the old /Default/ 301→404 produced.
        let (status, body) = response_status_and_body(Error::CalendarDiscoveryFailed(
            "PROPFIND https://caldav.fastmail.com/dav/calendars/user/u_name@fastmail.com/ returned 404".into()
        ))
        .await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert!(
            body.contains("calendar"),
            "body must reference the calendar problem: {body}"
        );
        assert!(
            body.contains("Settings"),
            "body must point at Settings: {body}"
        );
        assert!(
            !body.contains("u_name@fastmail.com"),
            "operator detail (account path) must not leak: {body}"
        );
        assert!(
            !body.contains("PROPFIND"),
            "operator detail (which step) must not leak: {body}"
        );
    }

    #[tokio::test]
    async fn rate_limited_returns_429() {
        let (status, body) =
            response_status_and_body(Error::RateLimited { retry_after: None }).await;
        assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
        assert!(body.contains("rate limited"));
    }

    #[tokio::test]
    async fn rate_limited_echoes_retry_after_header() {
        let err = Error::RateLimited {
            retry_after: Some(Duration::from_secs(7)),
        };
        let resp = err.into_response();
        assert_eq!(resp.status(), StatusCode::TOO_MANY_REQUESTS);
        let header = resp
            .headers()
            .get("retry-after")
            .expect("retry-after header present")
            .to_str()
            .unwrap();
        assert_eq!(header, "7");
    }

    #[tokio::test]
    async fn rate_limited_no_retry_after_omits_header() {
        let err = Error::RateLimited { retry_after: None };
        let resp = err.into_response();
        assert!(resp.headers().get("retry-after").is_none());
    }
}
