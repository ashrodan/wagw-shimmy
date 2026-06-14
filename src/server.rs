//! axum wiring: shared state, route table, and the two mapping handlers.
//!
//! Routes:
//! - `POST /webhook/gowa` — inbound: HMAC-verify raw bytes → build model → dedup-check → policy →
//!   **durably enqueue** → mark dedup → **ack 200**; a bounded worker forwards to the agent.
//! - `POST /send`        — outbound: bearer-verify → rate-limit → GOWA `/send/message`, record
//!   the returned id for reply-to-bot detection.
//! - `GET /livez`        — process liveness (static); `/healthz` is an alias.
//! - `GET /readyz`       — dependency-aware readiness (probes GOWA, optionally the agent).

use axum::{
    Json, Router,
    body::Bytes,
    extract::{DefaultBodyLimit, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use serde::Deserialize;
use serde_json::{Value, json};
use std::sync::Arc;
use subtle::ConstantTimeEq;

use crate::{
    agent::AgentClient,
    config::Config,
    dedup::TtlSet,
    error::{DynError, HttpError},
    forward::{ForwardQueue, ForwardWorker, WorkerConfig},
    gowa::{GowaClient, verify_signature},
    model::GowaEnvelope,
    policy,
    ratelimit::SendLimiter,
    sent_ids::SentIds,
};

/// Inbound dedup window: long enough to cover GOWA's 5× exponential-backoff retry train, short
/// enough to bound memory. GOWA's retries fit comfortably inside 10 minutes.
const DEDUP_TTL: std::time::Duration = std::time::Duration::from_secs(600);
const DEDUP_CAPACITY: usize = 50_000;

/// Everything a handler needs, cloneable (clients are `Arc`-backed; caches are shared via `Arc`).
#[derive(Clone)]
pub struct AppState {
    pub config: Arc<Config>,
    pub gowa: GowaClient,
    pub agent: AgentClient,
    pub dedup: Arc<TtlSet>,
    pub sent_ids: Arc<SentIds>,
    pub limiter: Arc<SendLimiter>,
    /// Durable inbound→agent forward queue; the worker (spawned via `spawn_forward_worker`) drains it.
    pub queue: ForwardQueue,
}

impl AppState {
    /// Build the shared state from a validated config. Constructs the HTTP clients once (shared
    /// connection pools), the bounded caches, and opens the durable forward queue on disk.
    pub fn new(config: Arc<Config>) -> Result<Self, DynError> {
        let gowa = GowaClient::new(&config)?;
        let agent = AgentClient::new(&config)?;
        let limiter = Arc::new(SendLimiter::per_minute(config.send_rate_per_min));
        let queue = ForwardQueue::new(&config.queue_dir)?;
        Ok(Self {
            config,
            gowa,
            agent,
            dedup: Arc::new(TtlSet::new(DEDUP_TTL, DEDUP_CAPACITY)),
            sent_ids: Arc::new(SentIds::new()),
            limiter,
            queue,
        })
    }

    /// Spawn the durable-forward worker. It drains anything already in `pending/` on startup, then
    /// forwards each enqueued inbound to the agent with bounded retries. Returns the handle so the
    /// caller can `shutdown()` it on SIGTERM; tests may drop it (the worker keeps running).
    pub fn spawn_forward_worker(&self) -> ForwardWorker {
        let worker_config = WorkerConfig::from_parts(
            self.config.forward_concurrency,
            self.config.forward_max_retries,
            self.config.forward_backoff,
        );
        ForwardWorker::spawn(self.queue.clone(), self.agent.clone(), worker_config)
    }
}

/// Upper bound on a request body. Inbound webhooks carry a single message (text/caption); outbound
/// sends carry one text. 256 KiB is far above any legitimate body and caps memory a localhost
/// peer (a compromised GOWA, or anything that reached loopback) could force us to buffer.
const MAX_BODY_BYTES: usize = 256 * 1024;

/// Assemble the router. Exposed so integration tests can drive it without binding a socket.
pub fn build_router(state: AppState) -> Router {
    Router::new()
        .route("/livez", get(livez))
        .route("/healthz", get(livez)) // alias kept for existing scripts/docs
        .route("/readyz", get(readyz))
        .route("/webhook/gowa", post(webhook_gowa))
        .route("/send", post(send))
        .layer(DefaultBodyLimit::max(MAX_BODY_BYTES))
        .with_state(state)
}

/// Process-only liveness: the binary is up and serving. Says nothing about dependencies.
async fn livez() -> impl IntoResponse {
    Json(json!({ "status": "ok" }))
}

/// Dependency-aware readiness: `200` only if the required dependencies answer within a short
/// timeout, else `503`. The body never carries credentials or raw upstream error bodies — just
/// per-dependency `ok` booleans. GOWA is a required dep; the agent is probed only when
/// `SHIM_READYZ_PROBE_AGENT` is set (it is a peered box, not localhost).
async fn readyz(State(state): State<AppState>) -> Response {
    let gowa_ok = state.gowa.ping().await;
    let (agent_value, agent_ok) = if state.config.readyz_probe_agent {
        let ok = state.agent.ping().await;
        (json!({ "ok": ok }), ok)
    } else {
        (json!({ "skipped": true }), true)
    };

    let ready = gowa_ok && agent_ok;
    let status = if ready {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    };
    let body = json!({
        "status": if ready { "ok" } else { "degraded" },
        "gowa": { "ok": gowa_ok },
        "agent": agent_value,
    });
    (status, Json(body)).into_response()
}

/// Inbound webhook from GOWA. Always responds fast — a 401 only when the HMAC fails, otherwise a
/// 200 ack regardless of whether the message was forwarded or dropped (so GOWA never retries a
/// message we deliberately ignored).
async fn webhook_gowa(State(state): State<AppState>, headers: HeaderMap, body: Bytes) -> Response {
    // 1. Verify HMAC over the RAW bytes before deserialising anything.
    let signature = headers
        .get("x-hub-signature-256")
        .and_then(|value| value.to_str().ok());
    if !verify_signature(
        state.config.gowa_webhook_secret.as_bytes(),
        &body,
        signature,
    ) {
        tracing::warn!("rejected GOWA webhook: bad or missing HMAC signature");
        return HttpError::Unauthorized.into_response();
    }

    // 2. Deserialise the verified bytes. A shape we can't parse is acked (drop, don't retry).
    let envelope: GowaEnvelope = match serde_json::from_slice(&body) {
        Ok(envelope) => envelope,
        Err(error) => {
            tracing::warn!(%error, "acked unparseable GOWA webhook");
            return StatusCode::OK.into_response();
        }
    };

    // 3. Structural validation → internal model (drops from_me / non-message / status-broadcast).
    let mut inbound = match envelope.into_inbound() {
        Ok(inbound) => inbound,
        Err(reason) => {
            tracing::debug!(?reason, "dropped inbound at structural stage");
            return StatusCode::OK.into_response();
        }
    };

    // 4. Mention = reply-to-bot: did this message quote one of our own recently-sent ids?
    inbound.mentioned = state.sent_ids.is_reply_to_bot(inbound.reply_to.as_deref());

    // 5. Fast-drop obvious GOWA re-deliveries before doing any work. This is a *check*, not a mark —
    //    the authoritative mark happens after enqueue (step 8). A duplicate that races past this
    //    check still lands on the same id-keyed queue file, so enqueue stays idempotent.
    if state.dedup.contains(&inbound.id) {
        tracing::info!(id = %inbound.id, "dropped duplicate GOWA delivery");
        return StatusCode::OK.into_response();
    }

    // 6. Policy.
    let decision = policy::evaluate(&state.config.policy, &inbound);
    if let Some(reason) = decision.drop_reason() {
        tracing::info!(id = %inbound.id, chat = %inbound.chat_id, reason, "dropped by policy");
        return StatusCode::OK.into_response();
    }

    // 7. Durably enqueue *before* acking. The worker forwards asynchronously, so the agent's LLM
    //    latency can't trip GOWA's 10s timeout — but unlike a fire-and-forget task, a failed forward
    //    survives agent downtime and shim restarts instead of being silently lost. If the enqueue
    //    itself fails (e.g. disk full) we do NOT ack, so GOWA retries.
    if let Err(error) = state.queue.enqueue(&inbound) {
        tracing::error!(id = %inbound.id, %error, "failed to enqueue inbound; not acking so GOWA retries");
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    }

    // 8. Mark the id seen (authoritative dedup) now that it is durably queued, then ack.
    state.dedup.insert_new(&inbound.id);
    tracing::info!(id = %inbound.id, chat = %inbound.chat_id, "enqueued inbound for forward");

    StatusCode::OK.into_response()
}

/// The agent's outbound send. `{to|chat_id, text|message, reply_to}` → GOWA `/send/message`.
async fn send(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Json<Value>, HttpError> {
    // 1. Bearer the agent must present.
    if !bearer_matches(&headers, &state.config.whatsapp_gateway_token) {
        return Err(HttpError::Unauthorized);
    }

    // 2. Parse after auth so unauthenticated callers learn nothing about the body shape.
    let request: SendRequest = serde_json::from_slice(&body)
        .map_err(|error| HttpError::BadRequest(format!("invalid send body: {error}")))?;
    let to = request
        .destination()
        .ok_or_else(|| HttpError::BadRequest("missing 'to'/'chat_id'".into()))?;
    let text = request
        .content()
        .ok_or_else(|| HttpError::BadRequest("missing 'text'/'message'".into()))?;

    // 3. Per-tenant outbound rate limit (ToS protection) — before touching GOWA.
    if !state.limiter.try_acquire() {
        tracing::warn!(to = %to, "outbound send rate-limited");
        return Err(HttpError::RateLimited);
    }

    // 4. Send. `to` carries the JID, so DM vs group is implicit (group `…@g.us` lands in the group).
    let reply_to = request
        .reply_to
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty());
    let message_id = state.gowa.send_message(to, text, reply_to).await?;

    // 5. Record our own id so a future reply-to-this resolves as a mention.
    if !message_id.is_empty() {
        state.sent_ids.record(&message_id);
    }

    Ok(Json(json!({ "sent": true, "id": message_id })))
}

/// The agent's send body. It sends `to` *and* `chat_id` (same value) plus `text` *and* `message`,
/// so we accept them as distinct optional fields and coalesce — using serde `alias` would trip a
/// duplicate-field error when both keys are present.
#[derive(Deserialize)]
struct SendRequest {
    #[serde(default)]
    to: Option<String>,
    #[serde(default)]
    chat_id: Option<String>,
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    message: Option<String>,
    #[serde(default)]
    reply_to: Option<String>,
}

impl SendRequest {
    /// Destination JID — `to` wins, then `chat_id`; trimmed, empty treated as absent.
    fn destination(&self) -> Option<&str> {
        first_non_empty([self.to.as_deref(), self.chat_id.as_deref()])
    }

    /// Message text — `text` wins, then `message`; trimmed, empty treated as absent.
    fn content(&self) -> Option<&str> {
        first_non_empty([self.text.as_deref(), self.message.as_deref()])
    }
}

fn first_non_empty<const N: usize>(candidates: [Option<&str>; N]) -> Option<&str> {
    candidates
        .into_iter()
        .flatten()
        .map(str::trim)
        .find(|value| !value.is_empty())
}

fn bearer_matches(headers: &HeaderMap, expected: &str) -> bool {
    headers
        .get("authorization")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .is_some_and(|token| constant_time_eq(token.as_bytes(), expected.as_bytes()))
}

/// Constant-time byte-string equality, so a token check doesn't leak how many leading bytes matched
/// via timing. Length is compared first (a non-secret); equal-length contents go through `subtle`.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.ct_eq(b).into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn constant_time_eq_matches_only_identical_bytes() {
        assert!(constant_time_eq(b"s3cr3t-token", b"s3cr3t-token"));
        assert!(!constant_time_eq(b"s3cr3t-token", b"s3cr3t-toke")); // length differs
        assert!(!constant_time_eq(b"s3cr3t-token", b"S3cr3t-token")); // one byte differs
        assert!(constant_time_eq(b"", b""));
    }

    #[test]
    fn bearer_matches_requires_exact_token() {
        let mut headers = HeaderMap::new();
        headers.insert("authorization", "Bearer right".parse().unwrap());
        assert!(bearer_matches(&headers, "right"));
        assert!(!bearer_matches(&headers, "wrong"));

        let mut no_prefix = HeaderMap::new();
        no_prefix.insert("authorization", "right".parse().unwrap()); // missing "Bearer "
        assert!(!bearer_matches(&no_prefix, "right"));

        assert!(!bearer_matches(&HeaderMap::new(), "right")); // no header
    }
}
