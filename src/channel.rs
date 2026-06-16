//! Per-group channel routing.
//!
//! One WhatsApp number (one GOWA + shim) can sit in several groups. By default every admitted
//! message is forwarded to a single agent endpoint. This module lets a tenant route specific groups
//! to *different* downstream agents ("channels"), while every forward also carries a **channel
//! label** so a downstream persona can be selected later even when two groups share a target.
//!
//! - [`ChannelConfig`] — one named channel: where to forward (`inbound_url`), how to probe it
//!   (`health_url`), and the bearer to present. The `"default"` channel is always synthesised from
//!   today's single target, so a config with no extra channels behaves exactly as before.
//! - [`ChannelRouter`] — built once at boot into `AppState`. Resolves a group JID → channel label
//!   ([`channel_for`]) and a label → [`AgentClient`] ([`client_for`]). A persisted label that is no
//!   longer configured falls back to the default client (with a `warn!`) rather than dead-lettering.
//!
//! The routing key travels *with the message*: the server stamps the resolved label onto
//! `Inbound.channel` before the durable enqueue, so a forward survives the queue and a restart and
//! still lands on the right target.

use std::collections::HashMap;

use crate::{agent::AgentClient, config::Config, error::DynError, model::Inbound};

/// The always-present default channel label. A group with no explicit mapping (and every DM) routes
/// here, and it points at today's single agent target.
pub const DEFAULT_CHANNEL: &str = "default";

/// One named downstream channel (target endpoint + credentials). Built in [`crate::config`] from the
/// `WA_CHANNEL_*` env contract; the `"default"` entry is synthesised from `AGENT_INBOUND_URL` +
/// `WHATSAPP_WEBHOOK_TOKEN`.
#[derive(Clone)]
pub struct ChannelConfig {
    pub label: String,
    /// Full inbound endpoint, e.g. `http://127.0.0.1:3002/whatsapp/inbound`.
    pub inbound_url: String,
    /// Readiness endpoint, e.g. `http://127.0.0.1:3002/health`.
    pub health_url: String,
    /// Bearer the shim presents to this channel's agent on forward.
    pub bearer: String,
}

/// Routes each admitted inbound to a downstream channel. Held in `AppState` as an `Arc`; the forward
/// worker consults it per message via the persisted `Inbound.channel` label.
pub struct ChannelRouter {
    /// label → client. Always contains [`DEFAULT_CHANNEL`].
    clients: HashMap<String, AgentClient>,
    /// group JID → channel label (only groups with an explicit mapping appear here).
    group_to_channel: HashMap<String, String>,
}

impl ChannelRouter {
    /// Build the per-channel clients and the group→label map from a validated [`Config`]. Reuses
    /// [`AgentClient::with_endpoint`] so every channel shares the same reqwest build (and the same
    /// debug-sink behaviour). `config.channels` always includes the synthesised default.
    pub fn from_config(config: &Config) -> Result<Self, DynError> {
        let mut clients = HashMap::with_capacity(config.channels.len());
        for channel in &config.channels {
            let client = AgentClient::with_endpoint(
                channel.inbound_url.clone(),
                channel.health_url.clone(),
                channel.bearer.clone(),
                config.agent_debug_sink,
            )?;
            clients.insert(channel.label.clone(), client);
        }
        let group_to_channel = config.group_channels.iter().cloned().collect();
        Ok(Self {
            clients,
            group_to_channel,
        })
    }

    /// Resolve the channel label for an inbound message: a group JID with an explicit mapping → its
    /// label; every unmapped group and every DM → [`DEFAULT_CHANNEL`].
    pub fn channel_for(&self, inbound: &Inbound) -> String {
        if inbound.is_group()
            && let Some(label) = self.group_to_channel.get(&inbound.chat_id)
        {
            return label.clone();
        }
        DEFAULT_CHANNEL.to_string()
    }

    /// The client for a persisted channel label. Falls back to the default client (and logs a
    /// `warn!`) when the label is no longer configured — e.g. a message enqueued under a channel that
    /// was removed from config before the worker drained it. A removed channel degrades to the
    /// default target rather than dead-lettering.
    pub fn client_for(&self, label: &str) -> &AgentClient {
        match self.clients.get(label) {
            Some(client) => client,
            None => {
                tracing::warn!(%label, "no client for channel label; forwarding to default");
                self.default_client()
            }
        }
    }

    /// The default channel's client (used by `/readyz`, which preserves today's single-target probe).
    pub fn default_client(&self) -> &AgentClient {
        self.clients
            .get(DEFAULT_CHANNEL)
            .expect("default channel client is always present")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn router(group_channels: Vec<(String, String)>, labels: &[&str]) -> ChannelRouter {
        // Build clients directly (no env) for every label plus the default.
        let mut clients = HashMap::new();
        let endpoint = |label: &str| {
            AgentClient::with_endpoint(
                format!("http://127.0.0.1:3001/whatsapp/inbound?{label}"),
                "http://127.0.0.1:3001/health".to_string(),
                "bearer".to_string(),
                false,
            )
            .unwrap()
        };
        clients.insert(DEFAULT_CHANNEL.to_string(), endpoint(DEFAULT_CHANNEL));
        for label in labels {
            clients.insert((*label).to_string(), endpoint(label));
        }
        ChannelRouter {
            clients,
            group_to_channel: group_channels.into_iter().collect(),
        }
    }

    fn inbound(chat_id: &str) -> Inbound {
        Inbound {
            chat_id: chat_id.into(),
            sender: chat_id.into(),
            body: "hi".into(),
            id: "m".into(),
            is_from_me: false,
            mentioned: false,
            reply_to: None,
            quoted_body: None,
            channel: DEFAULT_CHANNEL.into(),
        }
    }

    #[test]
    fn mapped_group_resolves_to_its_label() {
        let r = router(
            vec![("123-456@g.us".into(), "support".into())],
            &["support"],
        );
        assert_eq!(r.channel_for(&inbound("123-456@g.us")), "support");
    }

    #[test]
    fn unmapped_group_resolves_to_default() {
        let r = router(
            vec![("123-456@g.us".into(), "support".into())],
            &["support"],
        );
        assert_eq!(r.channel_for(&inbound("999-000@g.us")), DEFAULT_CHANNEL);
    }

    #[test]
    fn dm_always_resolves_to_default() {
        // Even if a DM JID somehow appeared as a mapping key, a DM never routes by group map.
        let r = router(
            vec![("61400111222@s.whatsapp.net".into(), "support".into())],
            &["support"],
        );
        assert_eq!(
            r.channel_for(&inbound("61400111222@s.whatsapp.net")),
            DEFAULT_CHANNEL
        );
    }

    #[test]
    fn client_for_falls_back_to_default_on_unknown_label() {
        let r = router(vec![], &["support"]);
        // A known label gets its own client; an unknown (e.g. removed) label falls back to default.
        let support = r.client_for("support");
        let unknown = r.client_for("was-removed");
        let default = r.default_client();
        assert!(std::ptr::eq(unknown, default));
        assert!(!std::ptr::eq(support, default));
    }
}
