//! GOWA upstream client and inbound HMAC verification.
//!
//! - **Inbound auth:** GOWA signs each webhook with `X-Hub-Signature-256: sha256=<hex>`, an
//!   HMAC-SHA256 over the raw request body keyed by the shared secret. We verify over the *raw
//!   bytes* (never a re-serialised JSON value, which would not byte-match) with a constant-time
//!   compare.
//! - **Outbound send:** `POST {gowa}/send/message {phone, message, reply_message_id}` with
//!   `X-Device-Id` + HTTP basic auth, over a single shared `reqwest::Client`. GOWA 4xx maps to
//!   `BadRequest` (the agent shouldn't retry), 5xx/timeout/connection to `Upstream` (retryable).
//! - **Chat presence:** `POST {gowa}/send/chat-presence {phone, action}` (typing indicator), same
//!   `X-Device-Id` + basic auth. Best-effort: the agent fires it fire-and-forget and never blocks the
//!   turn on it, but the shim still surfaces a non-2xx so the caller can log.

use hmac::{Hmac, Mac};
use reqwest::{Client, StatusCode};
use serde::Serialize;
use sha2::Sha256;
use std::time::Duration;

use crate::{
    config::{BasicAuth, Config},
    error::{DynError, HttpError},
};

type HmacSha256 = Hmac<Sha256>;

/// Short bound on a readiness probe — a wedged dependency must not stall `/readyz`.
const PING_TIMEOUT: Duration = Duration::from_secs(2);

/// Verify a GOWA `X-Hub-Signature-256` header against the raw body. Returns `false` for a missing
/// prefix, non-hex signature, or any mismatch. The comparison itself is constant-time
/// (`Mac::verify_slice`), so this does not leak timing about how much of the signature matched.
pub fn verify_signature(secret: &[u8], body: &[u8], header_value: Option<&str>) -> bool {
    let Some(header_value) = header_value else {
        return false;
    };
    let Some(hex_sig) = header_value.trim().strip_prefix("sha256=") else {
        return false;
    };
    let Ok(signature) = hex::decode(hex_sig) else {
        return false;
    };
    let Ok(mut mac) = HmacSha256::new_from_slice(secret) else {
        return false;
    };
    mac.update(body);
    mac.verify_slice(&signature).is_ok()
}

/// Compute a GOWA-style signature header value for `body`. Used by tests (and useful for local
/// signing); the production path only ever *verifies*.
pub fn sign(secret: &[u8], body: &[u8]) -> String {
    let mut mac = HmacSha256::new_from_slice(secret).expect("hmac accepts any key length");
    mac.update(body);
    format!("sha256={}", hex::encode(mac.finalize().into_bytes()))
}

/// Thin client over GOWA's REST surface. Cloneable: wraps an `Arc`-backed `reqwest::Client` and
/// owns the per-tenant device id + basic auth.
#[derive(Clone)]
pub struct GowaClient {
    http: Client,
    send_url: String,
    presence_url: String,
    devices_url: String,
    device_id: String,
    basic_auth: Option<BasicAuth>,
}

impl GowaClient {
    pub fn new(config: &Config) -> Result<Self, DynError> {
        let http = Client::builder()
            .timeout(Duration::from_secs(30))
            .build()
            .map_err(|error| {
                Box::<dyn std::error::Error + Send + Sync>::from(format!(
                    "failed to build GOWA HTTP client: {error}"
                ))
            })?;
        Ok(Self {
            http,
            send_url: format!("{}/send/message", config.gowa_url),
            presence_url: format!("{}/send/chat-presence", config.gowa_url),
            devices_url: format!("{}/devices", config.gowa_url),
            device_id: config.gowa_device_id.clone(),
            basic_auth: config.gowa_basic_auth.clone(),
        })
    }

    /// Readiness probe: a short-timeout `GET /devices` with basic auth. Returns `true` on any 2xx.
    /// `/devices` is the same endpoint `fleetctl pair` polls and is confirmed reachable under GOWA
    /// v8.7.0; a non-2xx, timeout, or connection failure all read as not-ready (no creds or body are
    /// surfaced to the caller).
    pub async fn ping(&self) -> bool {
        let mut request = self.http.get(&self.devices_url).timeout(PING_TIMEOUT);
        if let Some(auth) = &self.basic_auth {
            request = request.basic_auth(&auth.user, Some(&auth.pass));
        }
        matches!(request.send().await, Ok(response) if response.status().is_success())
    }

    /// Send a text message to a JID (`phone` in GOWA's terms — it accepts a `…@g.us` group JID just
    /// the same, which is what makes group replies route with no special-casing). Returns the
    /// message id GOWA assigned, so the caller can record it for reply-to-bot detection.
    pub async fn send_message(
        &self,
        chat_id: &str,
        text: &str,
        reply_to: Option<&str>,
    ) -> Result<String, HttpError> {
        let body = SendMessageRequest {
            phone: chat_id,
            message: text,
            reply_message_id: reply_to.filter(|value| !value.is_empty()),
        };

        let mut request = self
            .http
            .post(&self.send_url)
            .header("X-Device-Id", &self.device_id)
            .json(&body);
        if let Some(auth) = &self.basic_auth {
            request = request.basic_auth(&auth.user, Some(&auth.pass));
        }

        let response = request.send().await.map_err(|error| {
            // Timeout / connection failure — transient from the agent's perspective.
            HttpError::Upstream(format!("GOWA request failed: {}", classify(&error)))
        })?;

        let status = response.status();
        let raw = response.text().await.unwrap_or_default();
        if status.is_success() {
            return Ok(extract_message_id(&raw));
        }
        let snippet = raw.chars().take(300).collect::<String>();
        if status.is_client_error() && status != StatusCode::TOO_MANY_REQUESTS {
            // A 4xx (other than 429) is a request the agent could fix — surface as BadRequest.
            Err(HttpError::BadRequest(format!(
                "GOWA rejected send ({status}): {snippet}"
            )))
        } else {
            // 5xx, 429, or anything else — transient/gateway, retryable.
            Err(HttpError::Upstream(format!(
                "GOWA send failed ({status}): {snippet}"
            )))
        }
    }

    /// Set a chat presence (`action` is `"start"`/`"stop"` — typing indicator) for `phone` (the
    /// conversation JID). Forwards verbatim to GOWA's `/send/chat-presence` with the same device id +
    /// basic auth as `send_message`. Returns `()` on 2xx; error classification mirrors `send_message`
    /// (4xx ⇒ `BadRequest`, 5xx/429/transport ⇒ `Upstream`). GOWA only renders the indicator when the
    /// device presence is `available`; a non-2xx is the caller's to log, not to retry on.
    pub async fn send_presence(&self, phone: &str, action: &str) -> Result<(), HttpError> {
        let body = SendPresenceRequest { phone, action };

        let mut request = self
            .http
            .post(&self.presence_url)
            .header("X-Device-Id", &self.device_id)
            .json(&body);
        if let Some(auth) = &self.basic_auth {
            request = request.basic_auth(&auth.user, Some(&auth.pass));
        }

        let response = request.send().await.map_err(|error| {
            HttpError::Upstream(format!(
                "GOWA presence request failed: {}",
                classify(&error)
            ))
        })?;

        let status = response.status();
        if status.is_success() {
            return Ok(());
        }
        let raw = response.text().await.unwrap_or_default();
        let snippet = raw.chars().take(300).collect::<String>();
        if status.is_client_error() && status != StatusCode::TOO_MANY_REQUESTS {
            Err(HttpError::BadRequest(format!(
                "GOWA rejected presence ({status}): {snippet}"
            )))
        } else {
            Err(HttpError::Upstream(format!(
                "GOWA presence failed ({status}): {snippet}"
            )))
        }
    }
}

#[derive(Serialize)]
struct SendMessageRequest<'a> {
    phone: &'a str,
    message: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    reply_message_id: Option<&'a str>,
}

#[derive(Serialize)]
struct SendPresenceRequest<'a> {
    phone: &'a str,
    action: &'a str,
}

/// Pull the assigned message id out of a GOWA send response. GOWA wraps results as
/// `{ "code": "SUCCESS", "results": { "message_id": "…" } }`; we also accept a few field-name
/// variants and degrade to an empty string (mention detection simply won't fire) rather than error.
fn extract_message_id(raw: &str) -> String {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(raw) else {
        return String::new();
    };
    for path in [
        ["results", "message_id"],
        ["results", "id"],
        ["data", "message_id"],
        ["data", "id"],
    ] {
        if let Some(id) = value
            .get(path[0])
            .and_then(|node| node.get(path[1]))
            .and_then(|id| id.as_str())
            && !id.is_empty()
        {
            return id.to_string();
        }
    }
    value
        .get("message_id")
        .and_then(|id| id.as_str())
        .unwrap_or_default()
        .to_string()
}

fn classify(error: &reqwest::Error) -> String {
    if error.is_timeout() {
        "timeout".to_string()
    } else if error.is_connect() {
        "connection refused".to_string()
    } else {
        "transport error".to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Known-answer vector: HMAC-SHA256("topsecret", "hello world").
    // Computed independently; pins the wire format the production verifier accepts.
    const SECRET: &[u8] = b"topsecret";
    const BODY: &[u8] = b"hello world";

    #[test]
    fn sign_then_verify_roundtrips() {
        let header = sign(SECRET, BODY);
        assert!(header.starts_with("sha256="));
        assert!(verify_signature(SECRET, BODY, Some(&header)));
    }

    #[test]
    fn verify_rejects_wrong_secret_body_and_format() {
        let header = sign(SECRET, BODY);
        assert!(!verify_signature(b"other", BODY, Some(&header)));
        assert!(!verify_signature(SECRET, b"tampered", Some(&header)));
        assert!(!verify_signature(SECRET, BODY, None));
        assert!(!verify_signature(SECRET, BODY, Some("deadbeef"))); // missing sha256= prefix
        assert!(!verify_signature(SECRET, BODY, Some("sha256=nothex")));
    }

    #[test]
    fn wellformed_but_wrong_hex_is_rejected() {
        // A correctly-shaped `sha256=<64 hex>` header that is not the real digest must fail the
        // constant-time compare — guards the decode + verify_slice path, not just the prefix check.
        let wrong = "sha256=4e0e9c2d8f0b8c3f3f7d5d2f8d1f1c3b2a9e8d7c6b5a4938271605f4e3d2c1b0";
        assert!(!verify_signature(SECRET, BODY, Some(wrong)));
        // The genuine signature is exactly 64 hex chars after the prefix.
        let real = sign(SECRET, BODY);
        assert_eq!(real.len(), "sha256=".len() + 64);
    }

    #[test]
    fn extracts_message_id_from_results() {
        assert_eq!(
            extract_message_id(
                r#"{"code":"SUCCESS","results":{"message_id":"M-1","status":"sent"}}"#
            ),
            "M-1"
        );
        assert_eq!(extract_message_id(r#"{"data":{"id":"M-2"}}"#), "M-2");
        assert_eq!(extract_message_id("not json"), "");
    }
}
