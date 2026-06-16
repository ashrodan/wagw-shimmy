//! Wire types for the GOWA webhook envelope and the internal inbound model the shim works with.
//!
//! GOWA v8.7.0 delivers `{ event, device_id, payload }` (verified against
//! `vendor/gowa` `docs/webhook-payload.md`). The fields the shim reads off `payload`:
//!
//! | field            | meaning                                                              |
//! |------------------|----------------------------------------------------------------------|
//! | `chat_id`        | **conversation JID** — `…@g.us` (group) or `…@s.whatsapp.net` (DM).   |
//! |                  | This is what we echo back on `/send` so replies land in the right place. |
//! | `from`           | participant JID — used **only** for DM-sender allowlisting.           |
//! | `body`           | top-level text (also carries media captions); **not** `message.text`. |
//! | `id`             | message id — dedup key, and the value the agent echoes as `reply_to`. |
//! | `is_from_me`     | true for the bot's own echoed messages — dropped.                     |
//! | `replied_to_id`  | id of the message this one quotes; powers reply-to-bot detection.     |
//!
//! Deserialisation is deliberately lenient (every field optional, unknown fields ignored) so a
//! payload-shape drift on a GOWA bump degrades to a dropped message rather than a 500. Golden
//! fixtures in `tests/fixtures` pin the exact shapes we expect.

use serde::{Deserialize, Serialize};

/// The GOWA webhook envelope.
#[derive(Debug, Deserialize)]
pub struct GowaEnvelope {
    #[serde(default)]
    pub event: String,
    #[serde(default)]
    pub payload: GowaPayload,
}

/// The `payload` object inside a GOWA webhook. All fields optional for drift-tolerance.
#[derive(Debug, Default, Deserialize)]
pub struct GowaPayload {
    #[serde(default)]
    pub chat_id: Option<String>,
    #[serde(default)]
    pub from: Option<String>,
    #[serde(default)]
    pub body: Option<String>,
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub is_from_me: bool,
    #[serde(default)]
    pub replied_to_id: Option<String>,
    /// Text of the message this one quotes (a reply), if any. GOWA sets it from the quoted
    /// message's body (`event_message.go::buildMessageBody`). We prepend it to the forwarded body
    /// so a "reply + @tag" carries the question being replied to, not just the bare mention.
    #[serde(default)]
    pub quoted_body: Option<String>,
}

/// Why an inbound message was not forwarded. Returned by [`GowaEnvelope::into_inbound`] for the
/// structural drops (everything before policy), so the handler can log a precise reason.
#[derive(Debug, PartialEq, Eq)]
pub enum DropReason {
    NotAMessageEvent,
    FromMe,
    MissingChatId,
    MissingId,
    NonChat,
}

/// The shim's internal representation of an inbound message. Carries more than the forwarded
/// contract: `sender` (DM allowlisting), `mentioned` (reply-to-bot), and `reply_to` never leave
/// the shim — only `{chat_id, body, id, from_me}` is forwarded to the agent.
///
/// `Serialize`/`Deserialize` are derived so the durable forward queue (`crate::forward`) can
/// persist a pending message to disk and reload it verbatim after an agent outage or shim restart.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Inbound {
    /// Conversation JID — echoed back on `/send` so DM/group replies route correctly.
    pub chat_id: String,
    /// Participant JID — used only for DM-sender allowlisting.
    pub sender: String,
    pub body: String,
    pub id: String,
    pub is_from_me: bool,
    /// True when the message quotes one of the bot's own recently-sent ids (reply-to-bot mention).
    pub mentioned: bool,
    /// The id of the quoted message, if any.
    pub reply_to: Option<String>,
    /// Text of the quoted message, if this is a reply. Prepended to the forwarded body by
    /// [`Inbound::agent_body`] so the agent sees the context the user replied to. `#[serde(default)]`
    /// so a pending queue file written before this field existed still loads (as `None`).
    #[serde(default)]
    pub quoted_body: Option<String>,
    /// The downstream channel label this message routes to. Initialised to `"default"` here; the
    /// server overwrites it with the resolved label (see [`crate::channel::ChannelRouter`]) after
    /// policy passes, *before* the durable enqueue, so the routing decision survives the queue and a
    /// restart. `#[serde(default)]` so a pending file written before this field existed still loads
    /// (it falls back to `"default"`, i.e. today's behaviour) instead of dead-lettering.
    #[serde(default = "default_channel_label")]
    pub channel: String,
}

/// The default value for [`Inbound::channel`]: the always-present default channel label.
fn default_channel_label() -> String {
    crate::channel::DEFAULT_CHANNEL.to_string()
}

impl Inbound {
    /// True when `chat_id` is a group JID (`…@g.us`).
    pub fn is_group(&self) -> bool {
        self.chat_id.ends_with(crate::config::GROUP_SUFFIX)
    }

    /// The message text presented to the agent. When this message quotes another (a reply), the
    /// quoted text is prepended as a `>`-quote block so the agent has the context the user replied
    /// to. Without this, a "reply + @tag" reaches the agent as a bare mention (e.g. `@61413118079`)
    /// with no content — the user's actual question lives in the *quoted* message, which arrives (if
    /// at all) as a separate, un-addressed message that policy drops. Falls back to the plain body
    /// when nothing is quoted, so non-reply messages are unchanged.
    pub fn agent_body(&self) -> String {
        match self.quoted_body.as_deref().map(str::trim) {
            Some(quoted) if !quoted.is_empty() => {
                let quoted_block = quoted
                    .lines()
                    .map(|line| format!("> {line}"))
                    .collect::<Vec<_>>()
                    .join("\n");
                if self.body.trim().is_empty() {
                    quoted_block
                } else {
                    format!("{quoted_block}\n\n{}", self.body)
                }
            }
            _ => self.body.clone(),
        }
    }
}

impl GowaEnvelope {
    /// Validate the structural preconditions and build the internal [`Inbound`], or return the
    /// reason it was dropped. `mentioned` is left `false` here — the handler fills it from the
    /// sent-id cache, which is runtime state this pure conversion has no business touching.
    pub fn into_inbound(self) -> Result<Inbound, DropReason> {
        if self.event != "message" {
            return Err(DropReason::NotAMessageEvent);
        }
        let payload = self.payload;
        if payload.is_from_me {
            return Err(DropReason::FromMe);
        }
        let chat_id = non_empty(payload.chat_id).ok_or(DropReason::MissingChatId)?;
        if is_non_chat(&chat_id) {
            return Err(DropReason::NonChat);
        }
        let id = non_empty(payload.id).ok_or(DropReason::MissingId)?;
        // A group message has a distinct participant `from`; a DM may omit it, in which case the
        // sender *is* the chat. Fall back so DM allowlisting always has a JID to match.
        let sender = non_empty(payload.from).unwrap_or_else(|| chat_id.clone());

        Ok(Inbound {
            chat_id,
            sender,
            body: payload.body.unwrap_or_default(),
            id,
            is_from_me: false,
            mentioned: false,
            reply_to: non_empty(payload.replied_to_id),
            quoted_body: non_empty(payload.quoted_body),
            channel: default_channel_label(),
        })
    }
}

/// Status broadcasts and newsletters are not conversational and must never be answered.
fn is_non_chat(chat_id: &str) -> bool {
    chat_id == "status@broadcast" || chat_id.ends_with("@newsletter")
}

fn non_empty(value: Option<String>) -> Option<String> {
    value
        .map(|item| item.trim().to_string())
        .filter(|item| !item.is_empty())
}

/// True when `body` contains an `@`-mention of `number` (the bot's own number, digits only).
///
/// GOWA rewrites a WhatsApp tag into the message text as `@<phone-number>` (vendored
/// `event_message.go::buildMessageBody`), so a group member tagging the bot shows up in `body` as
/// `@<self_number>`. We require the matched number to be followed by a non-digit (or end of string)
/// so `@614131180790` doesn't spuriously match self `@61413118079`. `number` empty ⇒ never matches.
pub fn body_mentions_number(body: &str, number: &str) -> bool {
    if number.is_empty() {
        return false;
    }
    let needle = format!("@{number}");
    let mut search_from = 0;
    while let Some(offset) = body[search_from..].find(&needle) {
        let end = search_from + offset + needle.len();
        match body[end..].chars().next() {
            // A trailing digit means we matched a prefix of a longer number — keep scanning.
            Some(next) if next.is_ascii_digit() => search_from = end,
            _ => return true,
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(raw: &str) -> GowaEnvelope {
        serde_json::from_str(raw).unwrap()
    }

    #[test]
    fn body_mention_detects_bot_tag() {
        let n = "61413118079";
        // GOWA-rewritten tag forms.
        assert!(body_mentions_number("@61413118079 what's the weather", n));
        assert!(body_mentions_number("hey @61413118079", n));
        assert!(body_mentions_number("@61413118079", n));
        // Not a mention of us.
        assert!(!body_mentions_number("just chatting, no tag", n));
        assert!(!body_mentions_number("@61400111222 hi", n));
        // Boundary: a longer number that merely starts with ours must not match.
        assert!(!body_mentions_number("@614131180790", n));
        // Empty self-number never matches.
        assert!(!body_mentions_number("@61413118079", ""));
    }

    #[test]
    fn maps_a_group_message() {
        let env = parse(
            r#"{"event":"message","device_id":"d","payload":{
                "chat_id":"123-456@g.us","from":"61400111222@s.whatsapp.net",
                "body":"hi there","id":"msg_1","is_from_me":false,"replied_to_id":"prev_9"}}"#,
        );
        let inbound = env.into_inbound().unwrap();
        assert_eq!(inbound.chat_id, "123-456@g.us");
        assert_eq!(inbound.sender, "61400111222@s.whatsapp.net");
        assert_eq!(inbound.body, "hi there");
        assert_eq!(inbound.id, "msg_1");
        assert_eq!(inbound.reply_to.as_deref(), Some("prev_9"));
        assert!(inbound.is_group());
    }

    #[test]
    fn parses_quoted_body_and_composes_agent_body() {
        // A reply-to + @tag: GOWA puts the typed mention in `body` and the replied-to message's
        // text in `quoted_body`. The agent must see both, with the quote prepended.
        let env = parse(
            r#"{"event":"message","payload":{
                "chat_id":"123-456@g.us","from":"61400111222@s.whatsapp.net",
                "body":"@61413118079","id":"m1","replied_to_id":"q9",
                "quoted_body":"did you know how to format tables in whatsapp?"}}"#,
        );
        let inbound = env.into_inbound().unwrap();
        assert_eq!(
            inbound.quoted_body.as_deref(),
            Some("did you know how to format tables in whatsapp?")
        );
        assert_eq!(
            inbound.agent_body(),
            "> did you know how to format tables in whatsapp?\n\n@61413118079"
        );
    }

    #[test]
    fn agent_body_without_quote_is_plain_body() {
        let env =
            parse(r#"{"event":"message","payload":{"chat_id":"x@g.us","body":"hello","id":"m"}}"#);
        let inbound = env.into_inbound().unwrap();
        assert!(inbound.quoted_body.is_none());
        assert_eq!(inbound.agent_body(), "hello");
    }

    #[test]
    fn agent_body_quote_only_when_body_empty() {
        // A reply with no added text (just quoting) → forward the quote alone, multi-line prefixed.
        let mut inbound =
            parse(r#"{"event":"message","payload":{"chat_id":"x@g.us","body":"","id":"m"}}"#)
                .into_inbound()
                .unwrap();
        inbound.quoted_body = Some("line one\nline two".to_string());
        assert_eq!(inbound.agent_body(), "> line one\n> line two");
    }

    #[test]
    fn dm_without_from_falls_back_to_chat_id() {
        let env = parse(
            r#"{"event":"message","payload":{"chat_id":"61400111222@s.whatsapp.net","body":"yo","id":"m2"}}"#,
        );
        let inbound = env.into_inbound().unwrap();
        assert_eq!(inbound.sender, "61400111222@s.whatsapp.net");
        assert!(!inbound.is_group());
    }

    #[test]
    fn drops_from_me() {
        let env = parse(
            r#"{"event":"message","payload":{"chat_id":"x@s.whatsapp.net","id":"m","is_from_me":true}}"#,
        );
        assert_eq!(env.into_inbound().unwrap_err(), DropReason::FromMe);
    }

    #[test]
    fn drops_non_message_event() {
        let env = parse(r#"{"event":"presence","payload":{}}"#);
        assert_eq!(
            env.into_inbound().unwrap_err(),
            DropReason::NotAMessageEvent
        );
    }

    #[test]
    fn drops_status_broadcast_and_newsletter() {
        let status =
            parse(r#"{"event":"message","payload":{"chat_id":"status@broadcast","id":"m"}}"#);
        assert_eq!(status.into_inbound().unwrap_err(), DropReason::NonChat);
        let news =
            parse(r#"{"event":"message","payload":{"chat_id":"12345@newsletter","id":"m"}}"#);
        assert_eq!(news.into_inbound().unwrap_err(), DropReason::NonChat);
    }
}
