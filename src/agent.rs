//! Agent downstream client. Forwards an inbound message to the Rust agent's
//! `POST /whatsapp/inbound` with the bearer the agent expects. The forwarded body is the *original
//! contract only* — `{chat_id, body, id, from_me}` — not the shim's richer internal model.
//!
//! This call is driven by the durable forward worker (`crate::forward`) **after** the webhook has
//! already been acked 200 (see `server.rs`). Its `Result` drives the worker's retry/dead-letter
//! decision rather than being propagated to GOWA, which must not see the agent's latency.

use reqwest::Client;
use serde::Serialize;
use std::time::Duration;

use crate::{config::Config, error::DynError, model::Inbound};

/// The exact JSON the agent's WhatsApp channel deserialises. Field names are fixed by that contract.
#[derive(Serialize)]
struct InboundForward<'a> {
    chat_id: &'a str,
    body: &'a str,
    id: &'a str,
    from_me: bool,
}

/// Cloneable client over the agent's inbound endpoint.
#[derive(Clone)]
pub struct AgentClient {
    http: Client,
    inbound_url: String,
    bearer: String,
}

impl AgentClient {
    pub fn new(config: &Config) -> Result<Self, DynError> {
        // A bounded timeout so a wedged agent doesn't pin a forward task forever. This is larger
        // than GOWA's 10s webhook timeout deliberately — we've already acked GOWA, so the agent is
        // free to take a full LLM turn; we just don't want an unbounded hang.
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
            inbound_url: config.agent_inbound_url.clone(),
            bearer: config.whatsapp_webhook_token.clone(),
        })
    }

    /// Forward an inbound message to the agent. Returns `Ok(())` on a 2xx; otherwise a descriptive
    /// error the caller logs. The shim never retries here — GOWA's own retry plus inbound dedup is
    /// the delivery-guarantee layer.
    pub async fn forward(&self, inbound: &Inbound) -> Result<(), DynError> {
        let body = InboundForward {
            chat_id: &inbound.chat_id,
            body: &inbound.body,
            id: &inbound.id,
            from_me: inbound.is_from_me,
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
            Ok(())
        } else {
            let snippet = response.text().await.unwrap_or_default();
            let snippet = snippet.chars().take(300).collect::<String>();
            Err(Box::<dyn std::error::Error + Send + Sync>::from(format!(
                "agent inbound returned {status}: {snippet}"
            )))
        }
    }
}
