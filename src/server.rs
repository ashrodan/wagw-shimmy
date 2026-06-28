//! axum wiring: shared state, route table, and the two mapping handlers.
//!
//! Routes:
//! - `POST /webhook/gowa` — inbound: HMAC-verify raw bytes → build model → dedup-check → policy →
//!   **durably enqueue** → mark dedup → **ack 200**; a bounded worker forwards to the agent.
//! - `POST /send`        — outbound: bearer-verify → rate-limit → GOWA `/send/message`, record
//!   the returned id for reply-to-bot detection.
//! - `POST /send/chat-presence` — typing indicator: bearer-verify → GOWA `/send/chat-presence`.
//!   Deliberately *not* rate-limited (it is not a message send, so it must not spend the
//!   `WA_SEND_RATE_PER_MIN` budget) and best-effort from the agent's side.
//! - `POST /send/{image,audio,file}` — outbound media: bearer-verify → rate-limit → GOWA
//!   `/send/{image,audio,file}` (a `media_url` GOWA fetches, or in-band `media_base64` uploaded as
//!   multipart). Records the returned id like `/send`.
//! - `GET /media/{token}` — inbound media proxy: bearer-verify → verify the stateless media token →
//!   proxy-stream the GOWA static file it authorises (the URL the agent receives inbound).
//! - `GET /livez`        — process liveness (static); `/healthz` is an alias.
//! - `GET /readyz`       — dependency-aware readiness (probes GOWA, optionally the agent).

use axum::{
    Json, Router,
    body::{Body, Bytes},
    extract::{DefaultBodyLimit, Path, State},
    http::{HeaderMap, StatusCode, header::CONTENT_TYPE},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use base64::{Engine, engine::general_purpose::STANDARD};
use serde::Deserialize;
use serde_json::{Value, json};
use std::sync::Arc;
use subtle::ConstantTimeEq;

use crate::{
    channel::ChannelRouter,
    config::Config,
    dedup::TtlSet,
    error::{DynError, HttpError},
    forward::{ForwardQueue, ForwardWorker, WorkerConfig},
    gowa::{GowaClient, MediaSource, SendMediaOpts, verify_signature},
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
    /// Per-group channel router (label → client + group→label map). Replaces the single agent
    /// client: `webhook_gowa` resolves a label, the forward worker forwards to its client.
    pub router: Arc<ChannelRouter>,
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
        let router = Arc::new(ChannelRouter::from_config(&config)?);
        let limiter = Arc::new(SendLimiter::per_minute(config.send_rate_per_min));
        let queue = ForwardQueue::new(&config.queue_dir)?;
        Ok(Self {
            config,
            gowa,
            router,
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
        ForwardWorker::spawn(self.queue.clone(), self.router.clone(), worker_config)
    }
}

/// Upper bound on a request body. Inbound webhooks carry a single message (text/caption); outbound
/// sends carry one text. 256 KiB is far above any legitimate body and caps memory a localhost
/// peer (a compromised GOWA, or anything that reached loopback) could force us to buffer.
const MAX_BODY_BYTES: usize = 256 * 1024;

/// Higher body cap for the media-upload routes (`/send/{image,audio,file}` with `media_base64`): a
/// base64 image/voice-note/document must fit, but the global 256 KiB cap stays for everything else.
const MAX_MEDIA_BODY_BYTES: usize = 16 * 1024 * 1024;

/// Assemble the router. Exposed so integration tests can drive it without binding a socket. The
/// media-upload routes carry their own larger body limit; every other route keeps the 256 KiB cap.
pub fn build_router(state: AppState) -> Router {
    let standard = Router::new()
        .route("/livez", get(livez))
        .route("/healthz", get(livez)) // alias kept for existing scripts/docs
        .route("/readyz", get(readyz))
        .route("/webhook/gowa", post(webhook_gowa))
        .route("/send", post(send))
        .route("/send/reaction", post(send_reaction))
        .route("/send/chat-presence", post(send_chat_presence))
        .route("/media/{token}", get(media))
        .layer(DefaultBodyLimit::max(MAX_BODY_BYTES));

    let media_uploads = Router::new()
        .route("/send/image", post(send_image))
        .route("/send/audio", post(send_audio))
        .route("/send/file", post(send_file))
        .layer(DefaultBodyLimit::max(MAX_MEDIA_BODY_BYTES));

    standard.merge(media_uploads).with_state(state)
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
    let (agent_value, agent_ok) = if state.config.agent_debug_sink {
        // No agent target — forwards go to the debug sink. Surface it so a monitor (or a human)
        // can't mistake a sink-mode box for one wired to a real agent.
        (json!({ "debug_sink": true }), true)
    } else if state.config.readyz_probe_agent {
        // Probe the default channel's client — preserves today's single-target readiness. Per-channel
        // probing is a noted future extension.
        let ok = state.router.default_client().ping().await;
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

    // 4. Mention = the bot was addressed: either an `@`-tag of our own number (GOWA writes it into
    //    the body as `@<self_number>`) OR this message quotes one of our own recently-sent ids
    //    (reply-to-bot, i.e. continuing a thread we're in).
    let tagged = state
        .config
        .self_number
        .as_deref()
        .is_some_and(|number| crate::model::body_mentions_number(&inbound.body, number));
    // A reply *or* a reaction that references one of the bot's own recently-sent ids counts as
    // addressing the bot — so a reaction to the bot's message in a require-mention group isn't
    // dropped by policy. `reacted_message_id` is `None` for a normal message, so this is a no-op there.
    inbound.mentioned = tagged
        || state.sent_ids.is_reply_to_bot(inbound.reply_to.as_deref())
        || state
            .sent_ids
            .is_reply_to_bot(inbound.reacted_message_id.as_deref());

    // Addressing verdict for a group message (debug only; booleans, never message content): why it
    // was or wasn't summoned. `@`-tag and reply-quote carry were both confirmed live on wagw-1.
    if inbound.is_group() {
        tracing::debug!(
            id = %inbound.id,
            chat = %inbound.chat_id,
            mentioned = inbound.mentioned,
            tagged,
            has_quote = inbound.quoted_body.is_some(),
            "group inbound addressing verdict"
        );
    }

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

    // 7. Resolve the downstream channel and stamp it on the message *before* enqueue, so the routing
    //    decision is persisted with the message and survives the durable queue and a shim restart.
    //    A mapped group → its channel; every unmapped group and every DM → "default".
    inbound.channel = state.router.channel_for(&inbound);

    // 8. Durably enqueue *before* acking. The worker forwards asynchronously, so the agent's LLM
    //    latency can't trip GOWA's 10s timeout — but unlike a fire-and-forget task, a failed forward
    //    survives agent downtime and shim restarts instead of being silently lost. If the enqueue
    //    itself fails (e.g. disk full) we do NOT ack, so GOWA retries.
    if let Err(error) = state.queue.enqueue(&inbound) {
        tracing::error!(id = %inbound.id, %error, "failed to enqueue inbound; not acking so GOWA retries");
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    }

    // 9. Mark the id seen (authoritative dedup) now that it is durably queued, then ack.
    state.dedup.insert_new(&inbound.id);
    tracing::info!(id = %inbound.id, chat = %inbound.chat_id, channel = %inbound.channel, "enqueued inbound for forward");

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

/// The agent's emoji reaction. `{to|chat_id, message_id, emoji}` → GOWA
/// `POST /message/{message_id}/reaction`. Same bearer as `/send` and, like a media send, it spends
/// the outbound rate-limit budget (a reaction is a visible WhatsApp action, ToS-relevant). An
/// empty/absent `emoji` removes the bot's previous reaction (GOWA treats empty as un-react). No id is
/// recorded for reply-to-bot detection — a reaction is not a message the agent gets replied to.
async fn send_reaction(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Json<Value>, HttpError> {
    // 1. Bearer the agent must present.
    if !bearer_matches(&headers, &state.config.whatsapp_gateway_token) {
        return Err(HttpError::Unauthorized);
    }

    // 2. Parse after auth so unauthenticated callers learn nothing about the body shape.
    let request: ReactionRequest = serde_json::from_slice(&body)
        .map_err(|error| HttpError::BadRequest(format!("invalid reaction body: {error}")))?;
    let to = request
        .destination()
        .ok_or_else(|| HttpError::BadRequest("missing 'to'/'chat_id'".into()))?;
    let message_id = request
        .target()
        .ok_or_else(|| HttpError::BadRequest("missing 'message_id'".into()))?;
    // Empty emoji is valid — it removes the bot's reaction. Trim surrounding whitespace only.
    let emoji = request.emoji.as_deref().unwrap_or_default().trim();

    // 3. Per-tenant outbound rate limit — a reaction spends the send budget (unlike chat-presence).
    if !state.limiter.try_acquire() {
        tracing::warn!(to = %to, "outbound reaction rate-limited");
        return Err(HttpError::RateLimited);
    }

    // 4. React. `to` carries the JID, so DM vs group is implicit (a group reaction lands in the group).
    state.gowa.send_reaction(to, message_id, emoji).await?;
    Ok(Json(json!({ "reacted": true })))
}

/// The agent's typing indicator. `{phone|chat_id, action}` → GOWA `/send/chat-presence`. Same bearer
/// as `/send`, but **not** rate-limited: a presence ping is not a message and must never consume the
/// `WA_SEND_RATE_PER_MIN` budget (the agent refreshes it every ~10s during a turn). `action` is
/// forwarded verbatim (`"start"`/`"stop"`); GOWA owns the enum.
async fn send_chat_presence(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Json<Value>, HttpError> {
    // 1. Same bearer the agent presents on `/send`.
    if !bearer_matches(&headers, &state.config.whatsapp_gateway_token) {
        return Err(HttpError::Unauthorized);
    }

    // 2. Parse after auth so unauthenticated callers learn nothing about the body shape.
    let request: PresenceRequest = serde_json::from_slice(&body)
        .map_err(|error| HttpError::BadRequest(format!("invalid presence body: {error}")))?;
    let phone = request
        .destination()
        .ok_or_else(|| HttpError::BadRequest("missing 'phone'/'chat_id'".into()))?;
    let action = request
        .action
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| HttpError::BadRequest("missing 'action'".into()))?;

    // 3. No rate-limit gate here (see the doc comment): presence is not a send.
    match state.gowa.send_presence(phone, action).await {
        Ok(()) => {
            tracing::info!(phone = %phone, action, "forwarded chat-presence to GOWA");
            Ok(Json(json!({ "presence": true })))
        }
        Err(error) => {
            tracing::warn!(phone = %phone, action, %error, "chat-presence forward to GOWA failed");
            Err(error)
        }
    }
}

/// The inbound media proxy. `GET /media/{token}` (bearer-gated, same token as `/send`) verifies the
/// stateless media token, then proxy-streams the GOWA static file it authorises. The token is an
/// HMAC over the GOWA-relative path (see [`crate::media`]), so a bad/garbage token is a 403 and the
/// proxy can never be steered outside `statics/media/`. A GOWA 404 (file expired/cleaned) is
/// forwarded as a 404; a GOWA transport failure surfaces as the usual upstream error.
async fn media(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(token): Path<String>,
) -> Response {
    if !bearer_matches(&headers, &state.config.whatsapp_gateway_token) {
        return HttpError::Unauthorized.into_response();
    }
    let Some(path) = crate::media::verify(state.config.gowa_webhook_secret.as_bytes(), &token)
    else {
        tracing::warn!("rejected /media request: bad or unverifiable token");
        return StatusCode::FORBIDDEN.into_response();
    };

    match state.gowa.fetch_static(&path).await {
        Ok(fetched) => {
            let mut response = Response::new(Body::from(fetched.body));
            *response.status_mut() = fetched.status;
            if let Some(content_type) = fetched.content_type
                && let Ok(value) = content_type.parse()
            {
                response.headers_mut().insert(CONTENT_TYPE, value);
            }
            response
        }
        Err(error) => error.into_response(),
    }
}

/// The three outbound media kinds, each mapping to a GOWA `/send/{kind}` endpoint.
#[derive(Clone, Copy)]
enum MediaKind {
    Image,
    Audio,
    File,
}

async fn send_image(
    state: State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Json<Value>, HttpError> {
    send_media(state, headers, body, MediaKind::Image).await
}

async fn send_audio(
    state: State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Json<Value>, HttpError> {
    send_media(state, headers, body, MediaKind::Audio).await
}

async fn send_file(
    state: State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Json<Value>, HttpError> {
    send_media(state, headers, body, MediaKind::File).await
}

/// Shared outbound media handler. `{to|chat_id, media_url|media_base64, caption?, filename?,
/// reply_to?, voice?}` → the matching `GowaClient` send. Like `/send`: bearer-checked, then
/// **rate-limited** (a media send spends the `WA_SEND_RATE_PER_MIN` budget, unlike presence), and the
/// returned id is recorded for reply-to-bot detection. `media_url` (GOWA fetches it) wins over
/// `media_base64` (uploaded as multipart).
async fn send_media(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
    kind: MediaKind,
) -> Result<Json<Value>, HttpError> {
    if !bearer_matches(&headers, &state.config.whatsapp_gateway_token) {
        return Err(HttpError::Unauthorized);
    }

    let request: SendMediaRequest = serde_json::from_slice(&body)
        .map_err(|error| HttpError::BadRequest(format!("invalid media send body: {error}")))?;
    let to = request
        .destination()
        .ok_or_else(|| HttpError::BadRequest("missing 'to'/'chat_id'".into()))?
        .to_string();

    // Build the source: a URL GOWA fetches, or in-band base64 bytes we upload as multipart.
    let source = if let Some(url) = first_non_empty([request.media_url.as_deref()]) {
        MediaSource::Url(url.to_string())
    } else if let Some(encoded) = first_non_empty([request.media_base64.as_deref()]) {
        let data = STANDARD
            .decode(encoded)
            .map_err(|error| HttpError::BadRequest(format!("invalid media_base64: {error}")))?;
        let filename = first_non_empty([request.filename.as_deref()])
            .map(str::to_string)
            .unwrap_or_else(|| default_filename(kind));
        MediaSource::Bytes {
            data,
            filename,
            mime: first_non_empty([request.mime.as_deref()]).map(str::to_string),
        }
    } else {
        return Err(HttpError::BadRequest(
            "missing 'media_url'/'media_base64'".into(),
        ));
    };

    // A media send is a real send → it spends the outbound budget (unlike chat-presence).
    if !state.limiter.try_acquire() {
        tracing::warn!(to = %to, "outbound media send rate-limited");
        return Err(HttpError::RateLimited);
    }

    let opts = SendMediaOpts {
        caption: first_non_empty([request.caption.as_deref()]).map(str::to_string),
        reply_to: first_non_empty([request.reply_to.as_deref()]).map(str::to_string),
        // `voice` (or its `ptt` alias) only affects audio; image/file ignore it.
        voice: request.voice || request.ptt,
    };

    let message_id = match kind {
        MediaKind::Image => state.gowa.send_image(&to, source, opts).await?,
        MediaKind::Audio => state.gowa.send_audio(&to, source, opts).await?,
        MediaKind::File => state.gowa.send_file(&to, source, opts).await?,
    };

    if !message_id.is_empty() {
        state.sent_ids.record(&message_id);
    }
    Ok(Json(json!({ "sent": true, "id": message_id })))
}

/// A fallback multipart filename for a base64 upload with no `filename` given.
fn default_filename(kind: MediaKind) -> String {
    match kind {
        MediaKind::Image => "image.jpg",
        MediaKind::Audio => "audio.ogg",
        MediaKind::File => "file.bin",
    }
    .to_string()
}

/// The agent's media-send body. Destination is coalesced like `/send` (`to|chat_id`); the bytes are
/// either a `media_url` GOWA fetches or an in-band `media_base64`. `voice`/`ptt` (audio voice note)
/// are accepted as aliases.
#[derive(Deserialize)]
struct SendMediaRequest {
    #[serde(default)]
    to: Option<String>,
    #[serde(default)]
    chat_id: Option<String>,
    #[serde(default)]
    media_url: Option<String>,
    #[serde(default)]
    media_base64: Option<String>,
    #[serde(default)]
    caption: Option<String>,
    #[serde(default)]
    filename: Option<String>,
    #[serde(default)]
    mime: Option<String>,
    #[serde(default)]
    reply_to: Option<String>,
    #[serde(default)]
    voice: bool,
    #[serde(default)]
    ptt: bool,
}

impl SendMediaRequest {
    /// Destination JID — `to` wins, then `chat_id`; trimmed, empty treated as absent.
    fn destination(&self) -> Option<&str> {
        first_non_empty([self.to.as_deref(), self.chat_id.as_deref()])
    }
}

/// The agent's reaction body. Destination is coalesced like `/send` (`to|chat_id`); `message_id` is
/// the message to react to; `emoji` is the reaction (empty/absent removes the bot's reaction).
#[derive(Deserialize)]
struct ReactionRequest {
    #[serde(default)]
    to: Option<String>,
    #[serde(default)]
    chat_id: Option<String>,
    #[serde(default)]
    message_id: Option<String>,
    #[serde(default)]
    emoji: Option<String>,
}

impl ReactionRequest {
    /// Destination JID — `to` wins, then `chat_id`; trimmed, empty treated as absent.
    fn destination(&self) -> Option<&str> {
        first_non_empty([self.to.as_deref(), self.chat_id.as_deref()])
    }

    /// The id of the message to react to; trimmed, empty treated as absent.
    fn target(&self) -> Option<&str> {
        first_non_empty([self.message_id.as_deref()])
    }
}

/// The agent's chat-presence body. It sends `chat_id` *and* `phone` (same value), mirroring the
/// belt-and-braces keys of the send body, plus `action`. We coalesce the destination the same way.
#[derive(Deserialize)]
struct PresenceRequest {
    #[serde(default)]
    phone: Option<String>,
    #[serde(default)]
    chat_id: Option<String>,
    #[serde(default)]
    action: Option<String>,
}

impl PresenceRequest {
    /// Destination JID — `phone` wins, then `chat_id`; trimmed, empty treated as absent.
    fn destination(&self) -> Option<&str> {
        first_non_empty([self.phone.as_deref(), self.chat_id.as_deref()])
    }
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

    #[test]
    fn presence_destination_prefers_phone_then_chat_id() {
        let both: PresenceRequest = serde_json::from_str(
            r#"{"phone":"p@s.whatsapp.net","chat_id":"c@g.us","action":"start"}"#,
        )
        .unwrap();
        assert_eq!(both.destination(), Some("p@s.whatsapp.net"));

        // chat_id alone is honoured; whitespace-only phone is treated as absent.
        let chat_only: PresenceRequest =
            serde_json::from_str(r#"{"phone":"  ","chat_id":"c@g.us","action":"stop"}"#).unwrap();
        assert_eq!(chat_only.destination(), Some("c@g.us"));

        // Neither present → no destination.
        let none: PresenceRequest = serde_json::from_str(r#"{"action":"start"}"#).unwrap();
        assert_eq!(none.destination(), None);
    }
}
