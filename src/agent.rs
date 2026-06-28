//! Agent downstream client. Forwards an inbound message to the Rust agent's
//! `POST /whatsapp/inbound` with the bearer the agent expects. The forwarded body is the contract
//! `{chat_id, body, id, from_me, channel}` — not the shim's richer internal model. `channel` is the
//! resolved per-group routing label (see [`crate::channel`]); the agent ignores it today.
//!
//! This call is driven by the durable forward worker (`crate::forward`) **after** the webhook has
//! already been acked 200 (see `server.rs`). Its `Result` drives the worker's retry/dead-letter
//! decision rather than being propagated to GOWA, which must not see the agent's latency.
//!
//! When `SHIM_DEBUG_SINK` is set, `forward` short-circuits to a logging sink (no agent target): it
//! records the contract it would have sent and returns `Ok`, so the queue drains without an agent.

use reqwest::Client;
use serde::Serialize;
use std::time::Duration;

use crate::{config::Config, error::DynError, model::Inbound};

/// The exact JSON the agent's WhatsApp channel deserialises. Field names are fixed by that contract.
/// `channel` is a new, additive field: the agent ignores unknown fields today (its request type is a
/// plain `Deserialize` with no `deny_unknown_fields`), so it can persona-differentiate on it later.
/// `media` is likewise additive and omitted entirely for a text-only message.
#[derive(Serialize)]
struct InboundForward<'a> {
    chat_id: &'a str,
    body: &'a str,
    id: &'a str,
    from_me: bool,
    channel: &'a str,
    /// Message kind discriminator. Omitted entirely for a normal text/media message (so today's
    /// forwards stay byte-identical), set to `"reaction"` for an emoji reaction.
    #[serde(rename = "type", skip_serializing_if = "Option::is_none")]
    kind: Option<&'a str>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    media: Vec<ForwardMedia>,
    /// The emoji, on a reaction (`""` means the sender removed their reaction). Omitted otherwise.
    #[serde(skip_serializing_if = "Option::is_none")]
    reaction: Option<&'a str>,
    /// The id of the message a reaction targets. Omitted otherwise.
    #[serde(skip_serializing_if = "Option::is_none")]
    reacted_message_id: Option<&'a str>,
}

/// One media attachment as the agent sees it: a `type`, a fetchable `url` (back at the shim's
/// `/media` proxy, bearer-gated), and best-effort `mime`/`filename`. The agent GETs `url` lazily to
/// pull the bytes — they are never inlined into this JSON or the durable queue.
#[derive(Serialize)]
struct ForwardMedia {
    r#type: String,
    url: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    mime: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    filename: Option<String>,
}

/// What [`AgentClient::forward`] actually did, so the worker logs the truth. In debug-sink mode the
/// forward is accepted-and-discarded (never POSTed), but still returns `Ok` so the durable queue
/// drains — distinguishing the two prevents the "forwarded inbound to agent" log line from lying.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ForwardOutcome {
    /// POSTed to the agent and got a 2xx.
    Forwarded,
    /// Debug-sink mode: logged and discarded, no agent POST.
    SinkDropped,
}

/// Cloneable client over the agent's inbound endpoint.
#[derive(Clone)]
pub struct AgentClient {
    http: Client,
    inbound_url: String,
    health_url: String,
    bearer: String,
    /// Debug sink mode: log the forward and succeed instead of calling the agent (see `Config`).
    debug_sink: bool,
    /// Base URL the agent fetches inbound media from (the shim's own `/media` proxy), e.g.
    /// `http://127.0.0.1:8080`. Forward URLs are `{media_base}/media/<signed-token>`.
    media_base: String,
    /// Key the `/media` token is signed with — the per-tenant `GOWA_WEBHOOK_SECRET` (reused; see
    /// [`crate::media`]). Held as bytes-able `String`; never logged.
    media_signing_key: String,
}

impl AgentClient {
    /// Build a client for one concrete endpoint (one channel target). The reqwest build is shared by
    /// every channel, so [`crate::channel::ChannelRouter`] constructs one client per channel through
    /// here. A bounded timeout so a wedged agent doesn't pin a forward task forever — larger than
    /// GOWA's 10s webhook timeout deliberately: we've already acked GOWA, so the agent is free to
    /// take a full LLM turn; we just don't want an unbounded hang.
    pub fn with_endpoint(
        inbound_url: String,
        health_url: String,
        bearer: String,
        debug_sink: bool,
        media_base: String,
        media_signing_key: String,
    ) -> Result<Self, DynError> {
        let http = Client::builder()
            .timeout(Duration::from_secs(120))
            .build()
            .map_err(|error| {
                Box::<dyn std::error::Error + Send + Sync>::from(format!(
                    "failed to build agent HTTP client: {error}"
                ))
            })?;
        Ok(Self {
            http,
            inbound_url,
            health_url,
            bearer,
            debug_sink,
            media_base,
            media_signing_key,
        })
    }

    /// Build the single-target client from config — equivalent to today's behaviour and to the
    /// default channel. Thin wrapper over [`AgentClient::with_endpoint`].
    pub fn new(config: &Config) -> Result<Self, DynError> {
        Self::with_endpoint(
            config.agent_inbound_url.clone(),
            config.agent_health_url.clone(),
            config.whatsapp_webhook_token.clone(),
            config.agent_debug_sink,
            config.media_base_url.clone(),
            config.gowa_webhook_secret.clone(),
        )
    }

    /// Build the forwarded media list for an inbound: each item's `/media` URL is signed *here, at
    /// forward time* (not stored), so a `SHIM_MEDIA_BASE_URL` change needs no re-enqueue and the
    /// token survives a restart (it's stateless — see [`crate::media`]).
    fn build_forward_media(&self, inbound: &Inbound) -> Vec<ForwardMedia> {
        inbound
            .media
            .iter()
            .map(|item| ForwardMedia {
                r#type: item.kind.clone(),
                url: format!(
                    "{}/media/{}",
                    self.media_base,
                    crate::media::sign(self.media_signing_key.as_bytes(), &item.gowa_path)
                ),
                mime: item.mime.clone(),
                filename: item.filename.clone(),
            })
            .collect()
    }

    /// Optional readiness probe: a short-timeout `GET /health` (the agent exposes `/health`, not
    /// `/healthz`). Returns `true` on any 2xx. Only called by `/readyz` when `SHIM_READYZ_PROBE_AGENT`
    /// is set, since the agent is now a peered box rather than localhost.
    pub async fn ping(&self) -> bool {
        let request = self
            .http
            .get(&self.health_url)
            .timeout(Duration::from_secs(2))
            .bearer_auth(&self.bearer);
        matches!(request.send().await, Ok(response) if response.status().is_success())
    }

    /// Forward an inbound message to the agent. Returns `Ok(())` on a 2xx; otherwise a descriptive
    /// error the caller logs. The shim never retries here — GOWA's own retry plus inbound dedup is
    /// the delivery-guarantee layer.
    pub async fn forward(&self, inbound: &Inbound) -> Result<ForwardOutcome, DynError> {
        // The forwarded text is the composed agent body: when the message is a reply, the quoted
        // text is prepended so the agent sees what the user was replying to, not just a bare @tag.
        let agent_body = inbound.agent_body();
        let media = self.build_forward_media(inbound);
        // A reaction forwards a `type:"reaction"` discriminator plus the emoji and target id; a
        // normal message omits all three (the agent sees exactly today's shape).
        let (kind, reaction, reacted_message_id) = match inbound.kind {
            crate::model::InboundKind::Reaction => (
                Some("reaction"),
                inbound.reaction.as_deref(),
                inbound.reacted_message_id.as_deref(),
            ),
            crate::model::InboundKind::Message => (None, None, None),
        };

        // Debug sink: no agent target. Log the exact contract that *would* be forwarded and report
        // success so the durable queue drains cleanly (nothing dead-letters). Validates the
        // GOWA⟷shim leg in isolation. `body` is message content, not a secret.
        if self.debug_sink {
            tracing::info!(
                chat_id = %inbound.chat_id,
                id = %inbound.id,
                from_me = inbound.is_from_me,
                channel = %inbound.channel,
                kind = kind.unwrap_or("message"),
                media = media.len(),
                body = %agent_body,
                "DEBUG SINK: accepted inbound (no agent target) — would forward this contract"
            );
            return Ok(ForwardOutcome::SinkDropped);
        }

        let body = InboundForward {
            chat_id: &inbound.chat_id,
            body: &agent_body,
            id: &inbound.id,
            from_me: inbound.is_from_me,
            channel: &inbound.channel,
            kind,
            media,
            reaction,
            reacted_message_id,
        };

        let response = self
            .http
            .post(&self.inbound_url)
            .bearer_auth(&self.bearer)
            .json(&body)
            .send()
            .await
            .map_err(|error| {
                Box::<dyn std::error::Error + Send + Sync>::from(format!(
                    "agent inbound request failed: {error}"
                ))
            })?;

        let status = response.status();
        if status.is_success() {
            Ok(ForwardOutcome::Forwarded)
        } else {
            let snippet = response.text().await.unwrap_or_default();
            let snippet = snippet.chars().take(300).collect::<String>();
            Err(Box::<dyn std::error::Error + Send + Sync>::from(format!(
                "agent inbound returned {status}: {snippet}"
            )))
        }
    }
}
