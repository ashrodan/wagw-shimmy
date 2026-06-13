//! Bounded-TTL cache of the bot's *own* outbound message ids. Powers reply-to-bot mention
//! detection: when an inbound message's `replied_to_id` matches an id we recently sent, the message
//! is treated as a mention (the user replied to the bot), so a require-mention group will answer it.
//!
//! This is the chosen "mention" semantics (no GOWA patch for `contextInfo.mentionedJid`): a fresh
//! `@mention` does *not* summon the bot, but replying to its message does — and a chat listed in
//! `WA_FREE_RESPONSE_CHATS` bypasses the requirement altogether.

use std::time::Duration;

use crate::dedup::TtlSet;

/// How long a sent id stays mention-eligible. Generous: conversations reply to old messages.
const SENT_ID_TTL: Duration = Duration::from_secs(60 * 60 * 6);
/// Upper bound on retained ids; old ones evict first.
const SENT_ID_CAPACITY: usize = 10_000;

/// A cache of recently-sent outbound message ids.
pub struct SentIds {
    inner: TtlSet,
}

impl Default for SentIds {
    fn default() -> Self {
        Self::new()
    }
}

impl SentIds {
    pub fn new() -> Self {
        Self {
            inner: TtlSet::new(SENT_ID_TTL, SENT_ID_CAPACITY),
        }
    }

    /// Record an id GOWA returned for a message we just sent.
    pub fn record(&self, id: &str) {
        if !id.is_empty() {
            self.inner.record(id);
        }
    }

    /// True if `reply_to` quotes one of our recently-sent messages.
    pub fn is_reply_to_bot(&self, reply_to: Option<&str>) -> bool {
        match reply_to {
            Some(id) if !id.is_empty() => self.inner.contains(id),
            _ => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognises_reply_to_a_sent_id() {
        let sent = SentIds::new();
        sent.record("out_1");
        assert!(sent.is_reply_to_bot(Some("out_1")));
        assert!(!sent.is_reply_to_bot(Some("out_2")));
        assert!(!sent.is_reply_to_bot(None));
        assert!(!sent.is_reply_to_bot(Some("")));
    }
}
