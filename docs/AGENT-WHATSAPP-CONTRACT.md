# Agent WhatsApp channel — requirements & contract

What a downstream agent (today: `spike-rust-agent`) must implement to receive WhatsApp traffic from
`wagw-shimmy`. **The shim is the source of truth for this contract.** This doc is normative: "MUST"
is a hard requirement, "SHOULD" is strongly recommended.

## Topology (read this first — avoids a rebuild)

```
WhatsApp  ⟷  GOWA (whatsmeow)  ──webhook──▶  wagw-shimmy  ──POST /whatsapp/inbound──▶  AGENT
  phone        :3000 (owns session)   HMAC      :8080 (policy,     Bearer WEBHOOK_TOKEN     (this contract)
                                                 dedup, queue)
                                          ◀────────── POST /send ──────────────────────
                                                 Bearer GATEWAY_TOKEN, agent echoes chat_id
```

**The "WhatsApp gateway / Web session" is GOWA + `wagw-shimmy` — already built and deployed on
`wagw-1`.** The agent does NOT own a WhatsApp session, QR/pairing, or a GOWA process. The agent is a
plain HTTP service that (1) receives normalized inbound messages and (2) calls the shim back to send
replies. (So the agent-side scoping option "build the gateway sidecar" is already done — skip it.)

## Current state (2026-06-16)

The transport is **already implemented and unit-tested** in `spike-rust-agent`:
- `channels/whatsapp.rs` — inbound parse (field aliases), webhook-token auth, `send_text` to the shim.
- `server.rs::whatsapp_inbound` — auth → normalize → `handle_session_message` → reply via the gateway.
- `Channels::from_env` mounts `POST /whatsapp/inbound` whenever `whatsapp ∈ AGENT_CHANNELS`; `/send`
  (agent→shim) is already supported.

**It is not enabled in prod**: the VM runs `AGENT_CHANNELS=telegram` (`deploy/vm.env.example:21`), so
the route is never mounted and `/whatsapp/inbound` returns 404. Confirmed live: both the old Hermes
endpoint and `spike-rust-agent.exe.xyz` 404 on `/whatsapp/inbound`.

## The contract (normative)

### Inbound — `POST /whatsapp/inbound` (shim → agent)

- **Auth:** the shim sends `Authorization: Bearer <WHATSAPP_WEBHOOK_TOKEN>`. The agent MUST verify it
  and 401 on mismatch. (The agent also accepts `X-Webhook-Token: <token>`; the shim uses Bearer.)
- **Request body:**

  | field | type | notes |
  |---|---|---|
  | `chat_id` | string | **conversation JID** — group `…@g.us` or DM `…@s.whatsapp.net`. The reply key. |
  | `body` | string | message text |
  | `id` | string | message id — idempotency key; usable as `reply_to` to quote |
  | `from_me` | bool | always `false` in practice (shim drops own echoes); MUST be tolerated |
  | `channel` | string | per-group routing label (`"default"` or e.g. `"support"`). New, additive. |

  The agent MUST ignore unknown fields (no `deny_unknown_fields`). Field aliases already accepted by
  the agent (`from`/`sender`, `message_id`, `text`/`message`) MAY remain but the shim only sends the
  five above.
- **Response:** any **2xx** = accepted. The shim treats non-2xx, a connection failure, or **no
  response within its 120 s forward timeout** as a failure → it retries with exponential backoff and
  then dead-letters. **The forward is at-least-once** (see idempotency requirement R2c).

### Outbound — `POST {WHATSAPP_GATEWAY_URL}{SEND_PATH}` (agent → shim), default `/send`

- **Auth:** the agent sends `Authorization: Bearer <WHATSAPP_GATEWAY_TOKEN>`.
- **Body:** `{ "chat_id": <JID>, "to": <same JID>, "text": <reply>, "message": <reply>, "reply_to": <id?> }`.
  The shim coalesces `to|chat_id` and `text|message`; `reply_to` is optional (quotes a message).
- **MUST echo `chat_id` verbatim** — the value received inbound. This is the one correctness
  invariant: it routes group replies to the group and DM replies to the DM. Never substitute the
  participant/sender JID.
- **Shim responses:** `200 {"sent":true,"id":"<gowa-msg-id>"}`; `401` bad bearer; `400` missing
  `to`/`text`; `429` rate-limited (`WA_SEND_RATE_PER_MIN`). The agent SHOULD treat `429`/`5xx` as
  retryable with backoff and SHOULD NOT spin.

### Health — `GET /health`

- MUST return 2xx. The shim's `/readyz` optionally probes `{AGENT_INBOUND_URL base}/health` (sending
  the webhook bearer, which `/health` MAY ignore).

### Token / env wiring

| env (on the agent) | meaning | must equal (on the shim) |
|---|---|---|
| `AGENT_CHANNELS` | MUST include `whatsapp` to mount the route | — |
| `WHATSAPP_GATEWAY_URL` | shim base URL to POST `/send` to | the shim's listener (`SHIM_BIND`) |
| `WHATSAPP_GATEWAY_SEND_PATH` | send path (default `/send`) | the shim's `/send` route |
| `WHATSAPP_GATEWAY_TOKEN` | bearer the agent presents on `/send` | `WHATSAPP_GATEWAY_TOKEN` |
| `WHATSAPP_WEBHOOK_TOKEN` | bearer the agent requires on inbound | `WHATSAPP_WEBHOOK_TOKEN` |

Direction check (commonly flipped): **webhook token** = shim→agent inbound auth; **gateway token** =
agent→shim `/send` auth. `deploy/render-env.sh` already emits all four into `agent.env`.

## Requirements

### R1 — Enable the channel (config only, no code)
- **R1a** `AGENT_CHANNELS` MUST include `whatsapp` (e.g. `telegram,whatsapp` to run both).
- **R1b** `WHATSAPP_GATEWAY_URL`, `WHATSAPP_GATEWAY_TOKEN`, `WHATSAPP_WEBHOOK_TOKEN` MUST be set and
  match the shim's values.
- **Acceptance:** startup banner lists `whatsapp (POST /whatsapp/inbound)`; `GET /health` 2xx; a
  signed inbound (below) returns 2xx and a reply lands in the chat.

### R2 — Handler parity with Telegram (code, in `server.rs::whatsapp_inbound`)
Today `whatsapp_inbound` runs the **full agent turn synchronously** before returning 200 — the shim's
`/send` POST blocks 30–60 s+. Telegram already does this correctly; WhatsApp SHOULD match:
- **R2a Ack-fast:** validate (auth + parse + non-empty sender/text) → return **200 immediately** →
  run the turn on a **detached task**.
- **R2b notify_user:** pass a `notify_user` notifier into `handle_session_message` (as Telegram does)
  so intermediate/streamed messages send via `gateway.send_text(chat_id, …, None)`.
- **R2c Idempotency:** **dedup on the inbound `id`** (Telegram dedups on `update_id`). Because the
  shim forward is at-least-once, a lost 200 after a completed turn would otherwise cause a duplicate
  turn + double reply. Drop a repeat `id` within a bounded TTL window.
- **Acceptance:** the inbound POST returns 200 in <1 s regardless of turn length; replaying the same
  `id` produces exactly one reply; a long turn (>120 s) no longer causes shim retries/dead-letters.

### R3 — Per-group persona via `channel` (future, optional)
- **R3a** Parse `channel: Option<String>` on the inbound (currently ignored).
- **R3b** Thread it into persona/prompt selection. Session continuity is keyed `whatsapp:<chat_id>`
  (per conversation) and SHOULD remain so; `channel` selects *behaviour*, not identity.
- **Acceptance:** two groups mapped to different channels on one number can be given different
  personas; unset/`"default"` behaves as today.

### R4 — Verification (no deploy)
- **R4a** Local smoke test: start the agent with `AGENT_CHANNELS=whatsapp` + a mock/echo `/send`
  target; POST a sample inbound with the bearer; assert 2xx, a turn ran, and `/send` was called with
  the **same `chat_id`** and a `reply_to` of the inbound `id`.
- Sample inbound:
  ```sh
  curl -fsS -X POST http://127.0.0.1:3001/whatsapp/inbound \
    -H "Authorization: Bearer $WHATSAPP_WEBHOOK_TOKEN" -H "Content-Type: application/json" \
    -d '{"chat_id":"120363428950046857@g.us","body":"ping","id":"TEST_1","from_me":false,"channel":"default"}'
  ```

## Out of scope (handled by the shim, not the agent)
- WhatsApp session / pairing / QR, GOWA, HMAC verification of GOWA webhooks.
- Inbound admission policy (DM/group allowlists, require-mention), GOWA-redelivery dedup, the durable
  forward queue, and outbound send rate-limiting — all in `wagw-shimmy`.
- Per-group routing *decision* (which group → which `channel`) — the shim resolves it and stamps the
  label; the agent only *consumes* `channel` (R3).
