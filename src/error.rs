use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use std::fmt;

#[derive(Debug)]
pub enum Error {
    Auth(String),
    Network(String),
    NotConnected,
    NotFound(String),
    BadRequest(String),
    Internal(String),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Auth(msg) => write!(f, "authentication failed: {msg}"),
            Error::Network(msg) => write!(f, "network error: {msg}"),
            Error::NotConnected => write!(f, "not connected to email server"),
            Error::NotFound(msg) => write!(f, "not found: {msg}"),
            Error::BadRequest(msg) => write!(f, "bad request: {msg}"),
            Error::Internal(msg) => write!(f, "internal error: {msg}"),
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
        let (status, client_message) = match &self {
            Error::Auth(_) => (StatusCode::UNAUTHORIZED, "authentication failed".into()),
            Error::NotFound(msg) => (StatusCode::NOT_FOUND, format!("not found: {msg}")),
            Error::BadRequest(msg) => (StatusCode::BAD_REQUEST, format!("bad request: {msg}")),
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
        };
        let body = serde_json::json!({ "error": client_message });
        (status, axum::Json(body)).into_response()
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
}
