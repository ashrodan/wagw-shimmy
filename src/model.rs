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

use serde::Deserialize;

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
#[derive(Debug, Clone, PartialEq, Eq)]
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
}

impl Inbound {
    /// True when `chat_id` is a group JID (`…@g.us`).
    pub fn is_group(&self) -> bool {
        self.chat_id.ends_with(crate::config::GROUP_SUFFIX)
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

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(raw: &str) -> GowaEnvelope {
        serde_json::from_str(raw).unwrap()
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
