# Group addressing — when the bot replies in a group

In a group the bot is **silent by default and only replies when addressed**. This doc explains the
admission model, how `@`-mention detection works (no GOWA patch), and how to tune it. DMs are
governed separately (`WA_DM_POLICY`/`WA_DM_ALLOW`) and are unaffected by anything here.

## The two gates a group message passes

A group message is forwarded to the agent only if it clears **both**:

1. **Admission** — is this group in scope? `WA_GROUP_POLICY` (`off` | `allowlist` | `open`) +
   `WA_GROUP_ALLOW` (group JIDs). A group mapped to a channel (`WA_GROUP_CHANNELS`) is implicitly
   allowlisted. This gates *which groups*.
2. **Addressing** — was the bot actually addressed? Governed by `WA_REQUIRE_MENTION` (**default
   `true`**). This gates *which messages within an admitted group*.

Both live in `src/policy.rs` (pure, unit-tested). Addressing is summarised by the `Inbound.mentioned`
flag, computed in `src/server.rs::webhook_gowa` before policy runs.

## What counts as "addressed"

With `WA_REQUIRE_MENTION=true`, a group message is forwarded only if **either**:

- **`@`-mention of the bot** — someone tags the bot's number. WhatsApp/GOWA rewrites a tag into the
  **message body** as `@<phone-number>` (vendored `event_message.go::buildMessageBody` resolves the
  tag's LID→PN), so a tag of the bot shows up as `@<WA_SELF_NUMBER>` (e.g. `@61413118079`). The shim
  checks whether `body` contains that string — `model::body_mentions_number`, boundary-aware so
  `@614131180790` does **not** match `@61413118079`. **No GOWA patch needed.** Requires
  `WA_SELF_NUMBER` to be set; unset ⇒ this path is off.
- **Reply-to-bot** — the message quotes/replies to one of the bot's own recently-sent messages
  (`replied_to_id` matches a sent id the shim cached). This continues a thread the bot is already in.

Anyone in an admitted group can summon the bot — there is intentionally **no per-user filter**.

> **Bootstrap note:** reply-to-bot alone can't *start* a group conversation (the bot must have spoken
> first). `@`-mention is what lets a member summon it fresh — so set `WA_SELF_NUMBER` if you want
> groups to be usable. Without it, the bot can only be woken by replying to something it already said.

## Configuration

| Env | Default | Meaning |
|---|---|---|
| `WA_SELF_NUMBER` | unset | Bot's own number (digits / `+`-prefixed / full JID accepted). Enables `@`-mention detection. |
| `WA_REQUIRE_MENTION` | **`true`** | `true` ⇒ reply only when addressed (above). `false` ⇒ reply to **every** message in an admitted group. |
| `WA_GROUP_POLICY` | `off` | `off` \| `allowlist` \| `open` — which groups are admitted at all. |
| `WA_GROUP_ALLOW` | — | Allowlisted group JIDs (`…@g.us`). |
| `WA_FREE_RESPONSE_CHATS` | — | Group JIDs that bypass `require_mention` entirely (reply-to-all for *those* groups only). |

Example (safe default — only answer when tagged or in-thread):
```sh
WA_SELF_NUMBER=61413118079
WA_REQUIRE_MENTION=true
WA_GROUP_POLICY=allowlist
WA_GROUP_ALLOW=120363428950046857@g.us
```

## Verifying on a live box

A **temporary** log line (added in `webhook_gowa`, to be removed after first confirmation) prints the
verdict for every group message so you can confirm the live `@`-tag format:
```
TEMP group inbound (verifying @-tag format) ... mentioned=true tagged=true body=@61413118079 ...
```
Tag the bot once and check `journalctl -u wagw-shimmy`. If `tagged=true` and `body` shows
`@<WA_SELF_NUMBER>`, detection is confirmed. If instead the body shows a raw `@<lid>` (GOWA couldn't
resolve the LID→phone number), `body_mentions_number` won't match — fall back to matching that LID,
or use `WA_FREE_RESPONSE_CHATS` for that group.

## Limitations / future

- `@`-mention relies on GOWA resolving the tag to the bot's phone number in the body. The store
  normally has the self LID↔PN mapping, but an unresolved tag would be missed (see verification).
- No per-sender allowlist for groups (deliberately dropped — anyone in an admitted group may summon).
- The agent keys sessions per `chat_id` (whole group = one conversation); it does not currently scope
  per participant. Out of scope for the shim.

## Code

- `src/policy.rs` — `evaluate_group` (admission + `require_mention` gate).
- `src/model.rs` — `body_mentions_number` (+ unit test `body_mention_detects_bot_tag`).
- `src/server.rs::webhook_gowa` — sets `inbound.mentioned = @-tag || reply-to-bot`; TEMP log.
- `src/config.rs` — `WA_SELF_NUMBER` (`self_number`, `normalise_self_number`), `WA_REQUIRE_MENTION`
  (`env_bool_default(..., true)`).
- `tests/e2e.rs` — `at_mention_summons_in_require_mention_group`,
  `plain_group_message_dropped_under_require_mention`, `reply_to_bot_summons_in_require_mention_group`.
