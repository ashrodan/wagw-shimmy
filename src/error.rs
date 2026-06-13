//! HTTP error type for the shim. Mirrors the agent crate's small, explicit error style: a flat
//! enum that knows how to turn itself into a response. **Never embed a secret in a variant** — the
//! `Display`/response body is logged and returned to callers.

use axum::{
    Json,
    http::StatusCode,
    response::{IntoResponse, Response},
};
use serde_json::json;
use std::{error::Error, fmt};

/// Boxed dynamic error used for internal plumbing (client construction, IO) where a typed
/// `HttpError` is not yet warranted. Matches `spike-rust-agent`'s `DynError`.
pub type DynError = Box<dyn Error + Send + Sync>;

/// The errors a request handler can surface. Each maps to a single HTTP status. The string payloads
/// are caller-safe descriptions only — request shape, not credentials.
#[derive(Debug)]
pub enum HttpError {
    /// 400 — the caller (agent) sent a malformed/unacceptable request, or GOWA rejected the send
    /// with a 4xx (a bad request the agent could fix).
    BadRequest(String),
    /// 401 — missing/incorrect bearer or HMAC signature.
    Unauthorized,
    /// 429 — per-tenant outbound rate limit tripped (ToS protection).
    RateLimited,
    /// 502 — GOWA (or the agent) failed transiently: 5xx, timeout, or connection error. The agent
    /// can retry these; it should not retry `BadRequest`.
    Upstream(String),
}

impl HttpError {
    fn status(&self) -> StatusCode {
        match self {
            HttpError::BadRequest(_) => StatusCode::BAD_REQUEST,
            HttpError::Unauthorized => StatusCode::UNAUTHORIZED,
            HttpError::RateLimited => StatusCode::TOO_MANY_REQUESTS,
            HttpError::Upstream(_) => StatusCode::BAD_GATEWAY,
        }
    }

    fn message(&self) -> &str {
        match self {
            HttpError::BadRequest(message) | HttpError::Upstream(message) => message,
            HttpError::Unauthorized => "unauthorized",
            HttpError::RateLimited => "rate limited",
        }
    }
}

impl fmt::Display for HttpError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}: {}", self.status(), self.message())
    }
}

impl Error for HttpError {}

impl IntoResponse for HttpError {
    fn into_response(self) -> Response {
        let status = self.status();
        let body = Json(json!({ "error": self.message() }));
        (status, body).into_response()
    }
}
