# wagw-hermes-adapter

Bridges `wagw-shimmy` ([`AGENT-WHATSAPP-CONTRACT`](../../docs/AGENT-WHATSAPP-CONTRACT.md)) to
**Hermes Agent profiles**. Hermes has no native `/whatsapp/inbound`; this ~200-line stdlib-Python
service is the agent-side of the contract, deployed on the Hermes box (`bird-tortoise.exe.xyz`,
:8000, live since 2026-07-04).

```
wagw-1 shim ──wagw1-to-hermes──▶ adapter :8000 ── `wagw chat -Q --resume <sid>` ──▶ Hermes profile
            ◀─hermes-to-wagw1── POST /send (chat_id echoed verbatim)
```

- **Inbound** `POST /whatsapp/inbound`: bearer auth (`WHATSAPP_WEBHOOK_TOKEN`), dedup on message
  `id` (15 min TTL), ack-fast 200, turn runs detached (contract R2a/R2c). Reactions and `from_me`
  are acked and dropped; media is acked with a "not supported" note folded into the prompt.
- **Turn**: `channel` label → Hermes profile wrapper via `CHANNEL_PROFILES` (e.g.
  `hermes:wagw,default:wagw`). Conversation continuity is per `chat_id`: the adapter captures the
  `session_id:` that `hermes chat -Q` prints (on **stderr**) and `--resume`s it next turn
  (`sessions.json` beside the adapter; stale sessions retry fresh). Turns for the same chat are
  serialized; different chats run concurrently.
- **Reply**: `POST {WHATSAPP_GATEWAY_URL}/send` with `WHATSAPP_GATEWAY_TOKEN`, echoing `chat_id`
  verbatim (the invariant), `reply_to` = inbound id. Retries 429/5xx with backoff.

## Deploy

```sh
scp adapter.py <box>:~/wagw-hermes-adapter/adapter.py
scp wagw-hermes-adapter.service <box>:/tmp/ && ssh <box> sudo mv /tmp/wagw-hermes-adapter.service /etc/systemd/system/
# /etc/wagw-hermes-adapter.env (0600 root): WHATSAPP_WEBHOOK_TOKEN, WHATSAPP_GATEWAY_TOKEN,
#   WHATSAPP_GATEWAY_URL=https://hermes-to-wagw1.int.exe.xyz, CHANNEL_PROFILES=hermes:wagw,default:wagw
ssh <box> sudo systemctl enable --now wagw-hermes-adapter
```

The Hermes profile is created with `hermes profile create <name> --clone` (copy `auth.json` into
the profile home — `--clone` doesn't — and strip legacy `WHATSAPP_*` from its `.env`). To give a
set of groups their own persona: create another profile, add a channel label in the tenant yaml +
shim env, and map `label:wrapper` in `CHANNEL_PROFILES` — same adapter, same peerings.
