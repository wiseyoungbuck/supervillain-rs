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
    RateLimited { retry_after: Option<Duration> },
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
            Error::Internal(msg) => write!(f, "internal error: {msg}"),
            Error::RateLimited { retry_after } => match retry_after {
                Some(d) => write!(f, "rate limited — retry after {}s", d.as_secs()),
                None => write!(f, "rate limited"),
            },
        }
    }
}

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
