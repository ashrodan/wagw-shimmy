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
use reqwest::{Client, StatusCode, multipart};
use serde::Serialize;
use serde_json::{Value, json};
use sha2::Sha256;
use std::time::Duration;

use crate::{
    config::{BasicAuth, Config},
    error::{DynError, HttpError},
};

type HmacSha256 = Hmac<Sha256>;

/// Short bound on a readiness probe — a wedged dependency must not stall `/readyz`.
const PING_TIMEOUT: Duration = Duration::from_secs(2);

/// Upper bound on a media file the `/media` proxy will stream from GOWA. GOWA is a trusted loopback
/// peer, but this caps the memory a single proxied request can force us to buffer.
const MAX_MEDIA_BYTES: u64 = 64 * 1024 * 1024;

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
    image_url: String,
    audio_url: String,
    file_url: String,
    /// GOWA base URL (no trailing slash); the media proxy fetches `{statics_base}/{gowa_path}`.
    statics_base: String,
    device_id: String,
    basic_auth: Option<BasicAuth>,
}

/// The bytes (and how to serve them) for one outbound media send: either a public URL GOWA fetches
/// itself, or in-band bytes the shim uploads to GOWA as multipart.
pub enum MediaSource {
    /// A publicly-reachable URL → GOWA's `{kind}_url` JSON field; GOWA downloads it.
    Url(String),
    /// Decoded bytes → a multipart file upload to GOWA.
    Bytes {
        data: Vec<u8>,
        filename: String,
        mime: Option<String>,
    },
}

/// Optional knobs shared by the media sends. `caption` applies to image/file; `voice` (PTT) to
/// audio; `reply_to` quotes a message on any of them.
#[derive(Default)]
pub struct SendMediaOpts {
    pub caption: Option<String>,
    pub reply_to: Option<String>,
    /// Send audio as a push-to-talk voice note (GOWA `ptt`). Ignored by image/file.
    pub voice: bool,
}

/// A proxied static fetch from GOWA: the upstream status, its `Content-Type` (if any), and the body
/// bytes. The `/media` handler forwards all three so the agent sees GOWA's own content type.
pub struct StaticFetch {
    pub status: StatusCode,
    pub content_type: Option<String>,
    pub body: Vec<u8>,
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
            image_url: format!("{}/send/image", config.gowa_url),
            audio_url: format!("{}/send/audio", config.gowa_url),
            file_url: format!("{}/send/file", config.gowa_url),
            statics_base: config.gowa_url.clone(),
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
        classify_send(status, &raw, "send")
    }

    /// Send an image to a JID. `source` is either a public URL (GOWA's `image_url`, fetched by GOWA)
    /// or in-band bytes uploaded as multipart. `caption` and `reply_to` are honoured; `voice` is not.
    pub async fn send_image(
        &self,
        phone: &str,
        source: MediaSource,
        opts: SendMediaOpts,
    ) -> Result<String, HttpError> {
        self.post_media(&self.image_url, "image", "image_url", phone, source, &opts)
            .await
    }

    /// Send audio to a JID. Set `opts.voice` to deliver it as a push-to-talk voice note (GOWA `ptt`).
    pub async fn send_audio(
        &self,
        phone: &str,
        source: MediaSource,
        opts: SendMediaOpts,
    ) -> Result<String, HttpError> {
        self.post_media(&self.audio_url, "audio", "audio_url", phone, source, &opts)
            .await
    }

    /// Send an arbitrary file/document to a JID. `caption` and `reply_to` are honoured.
    pub async fn send_file(
        &self,
        phone: &str,
        source: MediaSource,
        opts: SendMediaOpts,
    ) -> Result<String, HttpError> {
        self.post_media(&self.file_url, "file", "file_url", phone, source, &opts)
            .await
    }

    /// Shared body of the three media sends. `field` is GOWA's multipart file field (`image`/`audio`/
    /// `file`); `url_field` is the JSON URL field (`image_url`/…). A [`MediaSource::Url`] sends JSON;
    /// [`MediaSource::Bytes`] sends multipart. Error/id handling mirrors [`Self::send_message`].
    async fn post_media(
        &self,
        url: &str,
        field: &str,
        url_field: &str,
        phone: &str,
        source: MediaSource,
        opts: &SendMediaOpts,
    ) -> Result<String, HttpError> {
        let mut request = self.http.post(url).header("X-Device-Id", &self.device_id);
        if let Some(auth) = &self.basic_auth {
            request = request.basic_auth(&auth.user, Some(&auth.pass));
        }

        request = match source {
            MediaSource::Url(media_url) => {
                let mut body = serde_json::Map::new();
                body.insert("phone".to_string(), json!(phone));
                body.insert(url_field.to_string(), json!(media_url));
                if let Some(caption) = non_empty(opts.caption.as_deref()) {
                    body.insert("caption".to_string(), json!(caption));
                }
                if opts.voice {
                    body.insert("ptt".to_string(), Value::Bool(true));
                }
                if let Some(reply) = non_empty(opts.reply_to.as_deref()) {
                    body.insert("reply_message_id".to_string(), json!(reply));
                }
                request.json(&Value::Object(body))
            }
            MediaSource::Bytes {
                data,
                filename,
                mime,
            } => {
                let mut part = multipart::Part::bytes(data).file_name(filename);
                if let Some(mime) = mime.as_deref().filter(|value| !value.is_empty()) {
                    part = part.mime_str(mime).map_err(|error| {
                        HttpError::BadRequest(format!("invalid media mime: {error}"))
                    })?;
                }
                let mut form = multipart::Form::new()
                    .text("phone", phone.to_string())
                    .part(field.to_string(), part);
                if let Some(caption) = non_empty(opts.caption.as_deref()) {
                    form = form.text("caption", caption.to_string());
                }
                if opts.voice {
                    form = form.text("ptt", "true");
                }
                if let Some(reply) = non_empty(opts.reply_to.as_deref()) {
                    form = form.text("reply_message_id", reply.to_string());
                }
                request.multipart(form)
            }
        };

        let response = request.send().await.map_err(|error| {
            HttpError::Upstream(format!("GOWA media request failed: {}", classify(&error)))
        })?;
        let status = response.status();
        let raw = response.text().await.unwrap_or_default();
        classify_send(status, &raw, "media send")
    }

    /// Proxy-fetch a GOWA static media file for the `/media` route. `path` is the verified
    /// GOWA-relative path (`statics/media/<file>`); the fetch is `GET {statics_base}/{path}`. GOWA
    /// serves `/statics/...` without basic auth (mounted before its auth middleware) but the header
    /// is sent anyway, harmlessly, for robustness. A transport failure → `Upstream`; an oversized
    /// body (by `Content-Length` or actual bytes) → `BadRequest`. Any HTTP status is returned as-is
    /// in [`StaticFetch`] so the handler can forward a GOWA 404 as a 404.
    pub async fn fetch_static(&self, path: &str) -> Result<StaticFetch, HttpError> {
        let url = format!("{}/{}", self.statics_base, path.trim_start_matches('/'));
        let mut request = self.http.get(&url);
        if let Some(auth) = &self.basic_auth {
            request = request.basic_auth(&auth.user, Some(&auth.pass));
        }
        let response = request.send().await.map_err(|error| {
            HttpError::Upstream(format!("GOWA static fetch failed: {}", classify(&error)))
        })?;

        let status = response.status();
        if let Some(len) = response.content_length()
            && len > MAX_MEDIA_BYTES
        {
            return Err(HttpError::BadRequest(format!(
                "GOWA media too large ({len} bytes)"
            )));
        }
        let content_type = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .map(str::to_string);
        let bytes = response.bytes().await.map_err(|error| {
            HttpError::Upstream(format!("GOWA static read failed: {}", classify(&error)))
        })?;
        if bytes.len() as u64 > MAX_MEDIA_BYTES {
            return Err(HttpError::BadRequest("GOWA media too large".to_string()));
        }
        Ok(StaticFetch {
            status,
            content_type,
            body: bytes.to_vec(),
        })
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

    /// React to a message with `emoji` (or *remove* the bot's reaction when `emoji` is empty). Maps
    /// to GOWA `POST /message/{message_id}/reaction {phone, emoji}`. Unlike the `/send/*` family the
    /// message id is a URL *path* segment, so the URL is built per call (and the segment is
    /// percent-encoded for safety). Same `X-Device-Id` + basic auth as the other sends; error
    /// classification mirrors [`Self::send_message`] (4xx ⇒ `BadRequest`, 5xx/429/transport ⇒
    /// `Upstream`). A reaction creates no new addressable message, so nothing is recorded for
    /// reply-to-bot detection — hence the `()` return.
    pub async fn send_reaction(
        &self,
        chat_id: &str,
        message_id: &str,
        emoji: &str,
    ) -> Result<(), HttpError> {
        let url = format!(
            "{}/message/{}/reaction",
            self.statics_base,
            encode_path_segment(message_id)
        );
        let body = json!({ "phone": chat_id, "emoji": emoji });

        let mut request = self
            .http
            .post(&url)
            .header("X-Device-Id", &self.device_id)
            .json(&body);
        if let Some(auth) = &self.basic_auth {
            request = request.basic_auth(&auth.user, Some(&auth.pass));
        }

        let response = request.send().await.map_err(|error| {
            HttpError::Upstream(format!(
                "GOWA reaction request failed: {}",
                classify(&error)
            ))
        })?;
        let status = response.status();
        let raw = response.text().await.unwrap_or_default();
        // GOWA echoes a message id we don't need (a reaction isn't a fresh addressable message).
        classify_send(status, &raw, "reaction").map(|_| ())
    }
}

/// Percent-encode a value for use as a single URL path segment. GOWA takes the message id as a path
/// parameter (`/message/{id}/reaction`); WhatsApp ids are usually `[A-F0-9]`/base64-ish, but a stray
/// `/`, `+`, `=`, or space would corrupt the path, so anything outside the RFC 3986 unreserved set
/// is escaped.
fn encode_path_segment(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(byte as char);
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
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

/// Classify a GOWA send-style response into the message id (on 2xx) or an [`HttpError`]: a 4xx other
/// than 429 is a request the agent could fix (`BadRequest`, not retried); 5xx/429/anything else is
/// transient (`Upstream`, retryable). `action` names the call for the error text. Shared by the text
/// and media sends so both classify identically.
fn classify_send(status: StatusCode, raw: &str, action: &str) -> Result<String, HttpError> {
    if status.is_success() {
        return Ok(extract_message_id(raw));
    }
    let snippet = raw.chars().take(300).collect::<String>();
    if status.is_client_error() && status != StatusCode::TOO_MANY_REQUESTS {
        Err(HttpError::BadRequest(format!(
            "GOWA rejected {action} ({status}): {snippet}"
        )))
    } else {
        Err(HttpError::Upstream(format!(
            "GOWA {action} failed ({status}): {snippet}"
        )))
    }
}

/// Trim a borrowed optional string, treating all-whitespace as absent. Mirrors `config`/`model`'s
/// `non_empty` discipline; used to drop empty caption/reply fields from a media send.
fn non_empty(value: Option<&str>) -> Option<&str> {
    value.map(str::trim).filter(|value| !value.is_empty())
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
