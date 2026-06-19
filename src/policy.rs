//! Pure, unit-tested admission policy. Given the per-tenant [`PolicyConfig`] and an [`Inbound`],
//! decide whether the message reaches the agent. No IO, no clock, no rate limiting here — the
//! outbound send rate limit lives on the `/send` path (see `ratelimit.rs`), because it bounds what
//! the bot *says*, not what it *hears*.

use crate::{
    config::{DmPolicy, GroupPolicy, PolicyConfig},
    model::Inbound,
};

/// The outcome of evaluating a message against policy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    /// Forward to the agent.
    Allow,
    /// Drop, with a human-readable reason for logs (never a secret).
    Drop(&'static str),
}

impl Decision {
    pub fn is_allow(&self) -> bool {
        matches!(self, Decision::Allow)
    }

    pub fn drop_reason(&self) -> Option<&'static str> {
        match self {
            Decision::Drop(reason) => Some(reason),
            Decision::Allow => None,
        }
    }
}

/// Decide whether `inbound` should be forwarded. DMs are gated on `sender` (the participant JID);
/// groups are gated on `chat_id` (the group JID) plus mention semantics.
pub fn evaluate(config: &PolicyConfig, inbound: &Inbound) -> Decision {
    if inbound.is_group() {
        evaluate_group(config, inbound)
    } else {
        evaluate_dm(config, inbound)
    }
}

fn evaluate_dm(config: &PolicyConfig, inbound: &Inbound) -> Decision {
    match config.dm_policy {
        DmPolicy::Off => Decision::Drop("dm policy is off"),
        DmPolicy::Open => Decision::Allow,
        DmPolicy::Allowlist => {
            if config
                .dm_allow
                .iter()
                .any(|allowed| allowed == &inbound.sender)
            {
                Decision::Allow
            } else {
                Decision::Drop("dm sender not in allowlist")
            }
        }
    }
}

fn evaluate_group(config: &PolicyConfig, inbound: &Inbound) -> Decision {
    // 1. Is this group even in scope?
    match config.group_policy {
        GroupPolicy::Off => return Decision::Drop("group policy is off"),
        GroupPolicy::Allowlist => {
            if !config
                .group_allow
                .iter()
                .any(|allowed| allowed == &inbound.chat_id)
            {
                return Decision::Drop("group not in allowlist");
            }
        }
        GroupPolicy::Open => {}
    }

    // 2. Mention gating. A free-response chat bypasses it entirely.
    if config
        .free_response_chats
        .iter()
        .any(|chat| chat == &inbound.chat_id)
    {
        return Decision::Allow;
    }
    if config.require_mention && !inbound.mentioned {
        return Decision::Drop("group requires mention (reply-to-bot)");
    }
    Decision::Allow
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::PolicyConfig;

    fn dm(sender: &str) -> Inbound {
        Inbound {
            chat_id: sender.to_string(),
            sender: sender.to_string(),
            body: "hi".into(),
            id: "m".into(),
            is_from_me: false,
            mentioned: false,
            reply_to: None,
            quoted_body: None,
            channel: "default".into(),
            media: vec![],
        }
    }

    fn group(chat: &str, mentioned: bool) -> Inbound {
        Inbound {
            chat_id: chat.to_string(),
            sender: "61400111222@s.whatsapp.net".into(),
            body: "hi".into(),
            id: "m".into(),
            is_from_me: false,
            mentioned,
            reply_to: None,
            quoted_body: None,
            channel: "default".into(),
            media: vec![],
        }
    }

    fn config() -> PolicyConfig {
        PolicyConfig {
            dm_policy: DmPolicy::Allowlist,
            dm_allow: vec!["61400111222@s.whatsapp.net".into()],
            group_policy: GroupPolicy::Allowlist,
            group_allow: vec!["123-456@g.us".into()],
            require_mention: true,
            free_response_chats: vec![],
        }
    }

    #[test]
    fn dm_allowlist_admits_known_sender_only() {
        let cfg = config();
        assert!(evaluate(&cfg, &dm("61400111222@s.whatsapp.net")).is_allow());
        assert_eq!(
            evaluate(&cfg, &dm("61400999000@s.whatsapp.net")),
            Decision::Drop("dm sender not in allowlist")
        );
    }

    #[test]
    fn dm_off_drops_everyone() {
        let mut cfg = config();
        cfg.dm_policy = DmPolicy::Off;
        assert!(!evaluate(&cfg, &dm("61400111222@s.whatsapp.net")).is_allow());
    }

    #[test]
    fn dm_open_admits_anyone() {
        let mut cfg = config();
        cfg.dm_policy = DmPolicy::Open;
        assert!(evaluate(&cfg, &dm("61400999000@s.whatsapp.net")).is_allow());
    }

    #[test]
    fn group_not_in_allowlist_dropped() {
        let cfg = config();
        assert_eq!(
            evaluate(&cfg, &group("999-000@g.us", true)),
            Decision::Drop("group not in allowlist")
        );
    }

    #[test]
    fn allowed_group_requires_mention() {
        let cfg = config();
        // Unmentioned → dropped even in an allowed group.
        assert_eq!(
            evaluate(&cfg, &group("123-456@g.us", false)),
            Decision::Drop("group requires mention (reply-to-bot)")
        );
        // Replied-to-bot → allowed.
        assert!(evaluate(&cfg, &group("123-456@g.us", true)).is_allow());
    }

    #[test]
    fn free_response_chat_bypasses_mention() {
        let mut cfg = config();
        cfg.free_response_chats = vec!["123-456@g.us".into()];
        assert!(evaluate(&cfg, &group("123-456@g.us", false)).is_allow());
    }

    #[test]
    fn group_off_drops_even_allowlisted_and_mentioned() {
        let mut cfg = config();
        cfg.group_policy = GroupPolicy::Off;
        assert_eq!(
            evaluate(&cfg, &group("123-456@g.us", true)),
            Decision::Drop("group policy is off")
        );
    }

    #[test]
    fn group_open_with_mention_off_admits_any_group() {
        let mut cfg = config();
        cfg.group_policy = GroupPolicy::Open;
        cfg.require_mention = false;
        assert!(evaluate(&cfg, &group("any-other@g.us", false)).is_allow());
    }
}
