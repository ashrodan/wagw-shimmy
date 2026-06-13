//! axum wiring: shared state, route table, and the two mapping handlers.
//!
//! Routes:
//! - `POST /webhook/gowa` — inbound: HMAC-verify raw bytes → build model → dedup → policy →
//!   **ack 200 immediately**, forward to the agent in a spawned (drained) task.
//! - `POST /send`        — outbound: bearer-verify → rate-limit → GOWA `/send/message`, record
//!   the returned id for reply-to-bot detection.
//! - `GET /healthz`      — liveness.

use axum::{
    Json, Router,
    body::Bytes,
    extract::State,
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use serde::Deserialize;
use serde_json::{Value, json};
use std::sync::Arc;
use tokio_util::task::TaskTracker;

use crate::{
    agent::AgentClient,
    config::Config,
    dedup::TtlSet,
    error::{DynError, HttpError},
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
    /// Tracks spawned inbound-forward tasks so SIGTERM can drain them before exit.
    pub tasks: TaskTracker,
}

impl AppState {
    /// Build the shared state from a validated config. Constructs the HTTP clients once (shared
    /// connection pools) and the bounded caches.
    pub fn new(config: Arc<Config>) -> Result<Self, DynError> {
        let gowa = GowaClient::new(&config)?;
        let agent = AgentClient::new(&config)?;
        let limiter = Arc::new(SendLimiter::per_minute(config.send_rate_per_min));
        Ok(Self {
            config,
            gowa,
            agent,
            dedup: Arc::new(TtlSet::new(DEDUP_TTL, DEDUP_CAPACITY)),
            sent_ids: Arc::new(SentIds::new()),
            limiter,
            tasks: TaskTracker::new(),
        })
    }
}

/// Assemble the router. Exposed so integration tests can drive it without binding a socket.
pub fn build_router(state: AppState) -> Router {
    Router::new()
        .route("/healthz", get(healthz))
        .route("/webhook/gowa", post(webhook_gowa))
        .route("/send", post(send))
        .with_state(state)
}

async fn healthz() -> impl IntoResponse {
    Json(json!({ "status": "ok" }))
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

    // 5. Dedup on id — GOWA re-delivers; forward each id at most once per window.
    if !state.dedup.insert_new(&inbound.id) {
        tracing::info!(id = %inbound.id, "dropped duplicate GOWA delivery");
        return StatusCode::OK.into_response();
    }

    // 6. Policy.
    let decision = policy::evaluate(&state.config.policy, &inbound);
    if let Some(reason) = decision.drop_reason() {
        tracing::info!(id = %inbound.id, chat = %inbound.chat_id, reason, "dropped by policy");
        return StatusCode::OK.into_response();
    }

    // 7. Ack now; forward asynchronously so the agent's LLM latency can't trip GOWA's 10s timeout.
    let agent = state.agent.clone();
    state.tasks.spawn(async move {
        if let Err(error) = agent.forward(&inbound).await {
            tracing::warn!(id = %inbound.id, %error, "failed to forward inbound to agent");
        } else {
            tracing::info!(id = %inbound.id, chat = %inbound.chat_id, "forwarded inbound to agent");
        }
    });

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
        .is_some_and(|token| token == expected)
}
