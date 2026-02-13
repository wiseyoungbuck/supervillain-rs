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
