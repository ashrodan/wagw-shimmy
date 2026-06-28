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
    /// Inbound media attachments. GOWA's auto-download (`WHATSAPP_AUTO_DOWNLOAD_MEDIA=true`, the
    /// default) writes each to `statics/media/<file>` and reports it here. Polymorphic: a bare path
    /// string (`"image":"statics/media/x.jpg"`) or an object (`{"path":…,"caption":…}`); with
    /// auto-download off it's instead `{"url":"https://mmg.whatsapp.net/…"}` (a CDN URL needing
    /// media keys we don't have — skipped, see [`GowaEnvelope::into_inbound`]).
    #[serde(default)]
    pub image: Option<GowaMedia>,
    #[serde(default)]
    pub audio: Option<GowaMedia>,
    #[serde(default)]
    pub document: Option<GowaMedia>,
    /// The emoji on a `message.reaction` event (the dedicated WhatsApp "react to a message"
    /// feature). An empty string means the sender *removed* their reaction. Absent on a plain
    /// `message` event.
    #[serde(default)]
    pub reaction: Option<String>,
    /// On a `message.reaction` event, the id of the message being reacted to. Absent on a plain
    /// `message` event.
    #[serde(default)]
    pub reacted_message_id: Option<String>,
}

/// A GOWA inbound media field, in either shape GOWA emits. Untagged so serde tries each variant by
/// structure: a JSON string → [`GowaMedia::Path`]; a JSON object → [`GowaMedia::Object`].
#[derive(Debug, Deserialize)]
#[serde(untagged)]
pub enum GowaMedia {
    /// Bare local path form: `"image": "statics/media/x.jpg"` (no caption).
    Path(String),
    /// Object form: `"image": {"path": …, "caption": …}` (auto-download on) or
    /// `{"url": "https://mmg.whatsapp.net/…"}` (auto-download off — no local path).
    Object {
        #[serde(default)]
        path: Option<String>,
        #[serde(default)]
        url: Option<String>,
        #[serde(default)]
        caption: Option<String>,
    },
}

/// One inbound media attachment, internal to the shim and persisted on [`Inbound`]. Carries only the
/// GOWA-relative path (tiny) — never the bytes; the forward builds a signed `/media` URL from it at
/// forward time (see [`crate::agent`]). Derives `Serialize`/`Deserialize` so it survives the durable
/// queue and a restart, exactly like the rest of [`Inbound`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MediaItem {
    /// `"image"` | `"audio"` | `"document"` — the forwarded `type`.
    pub kind: String,
    /// GOWA-relative path under `statics/media/`, e.g. `statics/media/abc123.jpg`.
    pub gowa_path: String,
    /// MIME type inferred from the path extension; `None` when unknown (the proxy still streams it
    /// with GOWA's own `Content-Type`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mime: Option<String>,
    /// Final path component, e.g. `abc123.jpg`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub filename: Option<String>,
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

/// What kind of inbound this is. `Message` is the default (text and/or media); `Reaction` is a
/// WhatsApp emoji reaction to an earlier message. Forwarded to the agent as the `type` discriminator
/// (a normal message omits it, for byte-identical backward compatibility — see [`crate::agent`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum InboundKind {
    #[default]
    Message,
    Reaction,
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
    /// Inbound media attachments (image/audio/document). Empty for a text-only message.
    /// `#[serde(default)]` so a pending file written before this field existed still loads (as an
    /// empty vec), exactly like the `channel`/`quoted_body` precedent.
    #[serde(default)]
    pub media: Vec<MediaItem>,
    /// The kind of inbound: text/media (`Message`) or an emoji `Reaction`. `#[serde(default)]` so a
    /// pending file written before this field existed loads as `Message` (today's behaviour).
    #[serde(default)]
    pub kind: InboundKind,
    /// On a reaction, the emoji the sender applied (an empty string means they *removed* it). `None`
    /// for a normal message.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reaction: Option<String>,
    /// On a reaction, the id of the message being reacted to. `None` for a normal message. When this
    /// matches one of the bot's own recently-sent ids, the reaction counts as addressing the bot
    /// (see the mention logic in `server.rs`), so a reaction to the bot's message isn't dropped by a
    /// require-mention group policy.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reacted_message_id: Option<String>,
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
    /// reason it was dropped. Dispatches on the GOWA event: `message` (text/media) and
    /// `message.reaction` (emoji reaction) are accepted; everything else (call/ack/edit/revoke/…) is
    /// dropped as [`DropReason::NotAMessageEvent`]. `mentioned` is left `false` here — the handler
    /// fills it from the sent-id cache, which is runtime state this pure conversion has no business
    /// touching.
    pub fn into_inbound(self) -> Result<Inbound, DropReason> {
        match self.event.as_str() {
            "message" => self.payload.into_message(),
            "message.reaction" => self.payload.into_reaction(),
            _ => Err(DropReason::NotAMessageEvent),
        }
    }
}

impl GowaPayload {
    /// Build a text/media [`Inbound`] from a `message` event payload. Drops the bot's own echoes,
    /// status/newsletter chats, and payloads missing a chat id / message id.
    fn into_message(self) -> Result<Inbound, DropReason> {
        if self.is_from_me {
            return Err(DropReason::FromMe);
        }
        let chat_id = non_empty(self.chat_id).ok_or(DropReason::MissingChatId)?;
        if is_non_chat(&chat_id) {
            return Err(DropReason::NonChat);
        }
        let id = non_empty(self.id).ok_or(DropReason::MissingId)?;
        // A group message has a distinct participant `from`; a DM may omit it, in which case the
        // sender *is* the chat. Fall back so DM allowlisting always has a JID to match.
        let sender = non_empty(self.from).unwrap_or_else(|| chat_id.clone());

        // Collect present media, accumulating any media-object captions to fold into an empty body.
        let mut media = Vec::new();
        let mut captions: Vec<String> = Vec::new();
        for (kind, field) in [
            ("image", self.image),
            ("audio", self.audio),
            ("document", self.document),
        ] {
            if let Some((item, caption)) = extract_media(kind, field) {
                if let Some(caption) = caption {
                    captions.push(caption);
                }
                media.push(item);
            }
        }

        // GOWA usually puts a media caption in the top-level `body`; fold the media-object caption in
        // only when `body` is empty so we never double it.
        let mut body = self.body.unwrap_or_default();
        if body.trim().is_empty() && !captions.is_empty() {
            body = captions.join("\n");
        }

        Ok(Inbound {
            chat_id,
            sender,
            body,
            id,
            is_from_me: false,
            mentioned: false,
            reply_to: non_empty(self.replied_to_id),
            quoted_body: non_empty(self.quoted_body),
            channel: default_channel_label(),
            media,
            kind: InboundKind::Message,
            reaction: None,
            reacted_message_id: None,
        })
    }

    /// Build a reaction [`Inbound`] from a `message.reaction` event payload. Same structural drops as
    /// a message; carries no text/media. The emoji is preserved verbatim (including an empty string,
    /// which signals the sender *removed* their reaction) so the agent can tell react from un-react —
    /// only `kind == Message` has `reaction == None`.
    fn into_reaction(self) -> Result<Inbound, DropReason> {
        if self.is_from_me {
            return Err(DropReason::FromMe);
        }
        let chat_id = non_empty(self.chat_id).ok_or(DropReason::MissingChatId)?;
        if is_non_chat(&chat_id) {
            return Err(DropReason::NonChat);
        }
        // The reaction event carries its own id (dedup key); the *target* is `reacted_message_id`.
        let id = non_empty(self.id).ok_or(DropReason::MissingId)?;
        let sender = non_empty(self.from).unwrap_or_else(|| chat_id.clone());
        let reaction = self.reaction.map(|emoji| emoji.trim().to_string());

        Ok(Inbound {
            chat_id,
            sender,
            body: String::new(),
            id,
            is_from_me: false,
            mentioned: false,
            reply_to: None,
            quoted_body: None,
            channel: default_channel_label(),
            media: Vec::new(),
            kind: InboundKind::Reaction,
            reaction: Some(reaction.unwrap_or_default()),
            reacted_message_id: non_empty(self.reacted_message_id),
        })
    }
}

/// Turn one GOWA media field into a [`MediaItem`] plus its caption (if the object form carried one).
/// Returns `None` when the field is absent or carries only a CDN `url` (auto-download off) — that
/// form needs media keys the shim doesn't have, so it's a documented limitation, logged and skipped.
fn extract_media(kind: &str, field: Option<GowaMedia>) -> Option<(MediaItem, Option<String>)> {
    let (path, caption) = match field? {
        GowaMedia::Path(path) => (non_empty(Some(path)), None),
        GowaMedia::Object { path, url, caption } => {
            let path = non_empty(path);
            if path.is_none() && non_empty(url).is_some() {
                tracing::warn!(
                    media_kind = %kind,
                    "inbound media carries only a CDN url (auto-download off); skipping"
                );
            }
            (path, non_empty(caption))
        }
    };
    // Normalise a leading slash off so the path matches the `statics/media/` prefix the proxy checks.
    let path = path?.trim_start_matches('/').to_string();
    let (mime, filename) = infer_mime_and_filename(&path);
    Some((
        MediaItem {
            kind: kind.to_string(),
            gowa_path: path,
            mime,
            filename,
        },
        caption,
    ))
}

/// Infer `(mime, filename)` from a GOWA media path by its extension. Best-effort: an unknown
/// extension yields `None` mime (the `/media` proxy still streams it with GOWA's own `Content-Type`).
fn infer_mime_and_filename(path: &str) -> (Option<String>, Option<String>) {
    let filename = path
        .rsplit('/')
        .next()
        .filter(|name| !name.is_empty())
        .map(str::to_string);
    let mime = filename
        .as_deref()
        .and_then(|name| name.rsplit_once('.'))
        .and_then(|(_, ext)| mime_for_extension(&ext.to_ascii_lowercase()))
        .map(str::to_string);
    (mime, filename)
}

/// Map a lowercase file extension to a MIME type for the common image/audio/document kinds GOWA
/// auto-downloads. Anything unrecognised returns `None`.
fn mime_for_extension(ext: &str) -> Option<&'static str> {
    Some(match ext {
        "jpg" | "jpeg" => "image/jpeg",
        "png" => "image/png",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "ogg" | "oga" => "audio/ogg",
        "mp3" => "audio/mpeg",
        "m4a" => "audio/mp4",
        "aac" => "audio/aac",
        "wav" => "audio/wav",
        "pdf" => "application/pdf",
        "doc" => "application/msword",
        "docx" => "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
        "xls" => "application/vnd.ms-excel",
        "xlsx" => "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
        "ppt" => "application/vnd.ms-powerpoint",
        "pptx" => "application/vnd.openxmlformats-officedocument.presentationml.presentation",
        "txt" => "text/plain",
        "csv" => "text/csv",
        "zip" => "application/zip",
        _ => return None,
    })
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
    fn parses_image_object_form_and_folds_caption_into_empty_body() {
        // Object form with a caption and an empty top-level body → caption becomes the body.
        let env = parse(
            r#"{"event":"message","payload":{
                "chat_id":"61400111222@s.whatsapp.net","id":"m","body":"",
                "image":{"path":"statics/media/abc123.jpg","caption":"check this out"}}}"#,
        );
        let inbound = env.into_inbound().unwrap();
        assert_eq!(inbound.media.len(), 1);
        assert_eq!(inbound.media[0].kind, "image");
        assert_eq!(inbound.media[0].gowa_path, "statics/media/abc123.jpg");
        assert_eq!(inbound.media[0].mime.as_deref(), Some("image/jpeg"));
        assert_eq!(inbound.media[0].filename.as_deref(), Some("abc123.jpg"));
        assert_eq!(inbound.body, "check this out");
    }

    #[test]
    fn parses_image_string_form_without_caption() {
        let env = parse(
            r#"{"event":"message","payload":{
                "chat_id":"61400111222@s.whatsapp.net","id":"m",
                "image":"statics/media/photo.png"}}"#,
        );
        let inbound = env.into_inbound().unwrap();
        assert_eq!(inbound.media.len(), 1);
        assert_eq!(inbound.media[0].gowa_path, "statics/media/photo.png");
        assert_eq!(inbound.media[0].mime.as_deref(), Some("image/png"));
        assert!(inbound.body.is_empty());
    }

    #[test]
    fn parses_audio_voice_note() {
        let env = parse(
            r#"{"event":"message","payload":{
                "chat_id":"61400111222@s.whatsapp.net","id":"m",
                "audio":"statics/media/voice.ogg"}}"#,
        );
        let inbound = env.into_inbound().unwrap();
        assert_eq!(inbound.media.len(), 1);
        assert_eq!(inbound.media[0].kind, "audio");
        assert_eq!(inbound.media[0].mime.as_deref(), Some("audio/ogg"));
    }

    #[test]
    fn parses_document_and_keeps_existing_body() {
        // A non-empty body is kept; the document still attaches.
        let env = parse(
            r#"{"event":"message","payload":{
                "chat_id":"61400111222@s.whatsapp.net","id":"m","body":"the report",
                "document":{"path":"statics/media/report.pdf"}}}"#,
        );
        let inbound = env.into_inbound().unwrap();
        assert_eq!(inbound.media.len(), 1);
        assert_eq!(inbound.media[0].kind, "document");
        assert_eq!(inbound.media[0].mime.as_deref(), Some("application/pdf"));
        assert_eq!(inbound.body, "the report");
    }

    #[test]
    fn skips_cdn_url_only_media_when_auto_download_off() {
        // Auto-download off → object carries only a CDN url, no local path → skipped entirely.
        let env = parse(
            r#"{"event":"message","payload":{
                "chat_id":"61400111222@s.whatsapp.net","id":"m",
                "image":{"url":"https://mmg.whatsapp.net/d/f/xyz.enc"}}}"#,
        );
        let inbound = env.into_inbound().unwrap();
        assert!(inbound.media.is_empty());
    }

    #[test]
    fn text_only_message_has_no_media() {
        let env = parse(
            r#"{"event":"message","payload":{
                "chat_id":"61400111222@s.whatsapp.net","id":"m","body":"hi"}}"#,
        );
        assert!(env.into_inbound().unwrap().media.is_empty());
    }

    #[test]
    fn message_event_is_kind_message_with_no_reaction_fields() {
        let env = parse(
            r#"{"event":"message","payload":{
                "chat_id":"61400111222@s.whatsapp.net","id":"m","body":"hi"}}"#,
        );
        let inbound = env.into_inbound().unwrap();
        assert_eq!(inbound.kind, InboundKind::Message);
        assert!(inbound.reaction.is_none());
        assert!(inbound.reacted_message_id.is_none());
    }

    #[test]
    fn parses_reaction_event() {
        let env = parse(
            r#"{"event":"message.reaction","payload":{
                "chat_id":"61400111222@s.whatsapp.net","from":"61400111222@s.whatsapp.net",
                "id":"r1","reaction":"👍","reacted_message_id":"MSG_1"}}"#,
        );
        let inbound = env.into_inbound().unwrap();
        assert_eq!(inbound.kind, InboundKind::Reaction);
        assert_eq!(inbound.reaction.as_deref(), Some("👍"));
        assert_eq!(inbound.reacted_message_id.as_deref(), Some("MSG_1"));
        assert_eq!(inbound.id, "r1");
        assert!(inbound.body.is_empty());
        assert!(inbound.media.is_empty());
    }

    #[test]
    fn empty_reaction_is_preserved_as_unreact() {
        // An empty emoji means the user removed their reaction — it must survive as `Some("")`, not
        // collapse to `None` (which means "not a reaction").
        let env = parse(
            r#"{"event":"message.reaction","payload":{
                "chat_id":"61400111222@s.whatsapp.net","id":"r1","reaction":"","reacted_message_id":"MSG_1"}}"#,
        );
        let inbound = env.into_inbound().unwrap();
        assert_eq!(inbound.kind, InboundKind::Reaction);
        assert_eq!(inbound.reaction.as_deref(), Some(""));
    }

    #[test]
    fn drops_from_me_reaction() {
        let env = parse(
            r#"{"event":"message.reaction","payload":{
                "chat_id":"x@s.whatsapp.net","id":"r1","is_from_me":true,"reaction":"👍"}}"#,
        );
        assert_eq!(env.into_inbound().unwrap_err(), DropReason::FromMe);
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
