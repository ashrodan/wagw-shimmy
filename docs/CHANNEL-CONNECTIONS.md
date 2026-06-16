# Channel connections — exe.dev box-to-box wiring

How the WhatsApp **shim box** (GOWA + `wagw-shimmy`) and one or more **agent boxes**
(`spike-rust-agent`) reach each other over **exe.dev peerings**. This is the *network* companion to
[`AGENT-WHATSAPP-CONTRACT.md`](AGENT-WHATSAPP-CONTRACT.md) (the *HTTP* contract): the contract says
*what* the two services send; this doc says *how the packets get from one box to the other*, and how
to grow that to more boxes.

> Conventions: "shim box" = the VM running GOWA + `wagw-shimmy` for one WhatsApp number (today
> `wagw-1`). "Agent box" = a VM running `spike-rust-agent` (today `spike-rust-agent`). One WhatsApp
> number = one shim box. One shim box may talk to **several** agent boxes via channel routing.

## The pieces and their local ports (per box)

| Service | Box | Local bind | Role |
|---|---|---|---|
| GOWA (whatsmeow) | shim box | `127.0.0.1:3000` | owns the WhatsApp session; POSTs webhooks to the shim |
| `wagw-shimmy` | shim box | `127.0.0.1:8080` (`SHIM_BIND`) | policy, dedup, durable queue; the box's inbound+outbound edge |
| `spike-rust-agent` | agent box | `0.0.0.0:8000` (public on exe.dev) | mounts `POST /whatsapp/inbound`; replies via the shim's `/send` |

GOWA↔shim is **loopback on the shim box** (never leaves the VM). Only the **shim↔agent** traffic
crosses boxes — that is what the peerings carry.

## The exe.dev peering model (read this once)

An exe.dev peering is **one-directional and named**. Each peering:

- is **attached to a source VM** (the side that *initiates* the call),
- targets a **peer** (the destination VM),
- exposes a hostname `https://<peering-name>.int.exe.xyz/` that is **only resolvable/reachable from
  the attached source VM**, and proxies to a service on the peer.

So a peering is a *one-way door*. A request/response round-trip works over a single peering (the
response comes back on the same connection). But because **each side here initiates its own calls**
(the shim pushes inbound; the agent pushes replies), a full channel link needs **two peerings, one
per initiating direction**.

`.int.exe.xyz` hostnames are private to the attached VM — they will **time out from your laptop or
any other box** (this is expected, not a fault). Test them *from the source VM*.

## The two hops (single number ↔ single agent)

```
        peering: wagw1-to-agent   (attached to wagw-1, peer = spike-rust-agent)
        from wagw-1 → https://wagw1-to-agent.int.exe.xyz  →  agent :8000
  ┌─ vm: wagw-1 ──────────────┐                         ┌─ vm: spike-rust-agent ───────┐
  │ GOWA :3000                │   inbound  (Bearer       │                              │
  │   │ webhook (HMAC, loopbk)│   WEBHOOK_TOKEN)         │  POST /whatsapp/inbound :8000│
  │   ▼                       │  ───────────────────────▶  (verifies WEBHOOK_TOKEN)    │
  │ wagw-shimmy :8080  ◀──────┼──────────────────────────  agent run → reply           │
  │   (verifies GATEWAY_TOKEN)│   reply   (Bearer        │   POST {GATEWAY_URL}/send    │
  └───────────────────────────┘   GATEWAY_TOKEN)         └──────────────────────────────┘
        from spike-rust-agent → https://agent-to-wagw1.int.exe.xyz  →  shim :8080
        peering: agent-to-wagw1   (attached to spike-rust-agent, peer = wagw-1)
```

### Env + token mapping

| Hop | Who initiates | Source box env var | Value (current) | Reaches | Token (presented → verified) |
|---|---|---|---|---|---|
| **inbound** | shim → agent | `AGENT_INBOUND_URL` (on the **shim**) | `https://wagw1-to-agent.int.exe.xyz` | agent `POST /whatsapp/inbound` | `WHATSAPP_WEBHOOK_TOKEN` (shim → agent) |
| **reply** | agent → shim | `WHATSAPP_GATEWAY_URL` (on the **agent**) | `https://agent-to-wagw1.int.exe.xyz` | shim `POST /send` | `WHATSAPP_GATEWAY_TOKEN` (agent → shim) |

- The shim appends `/whatsapp/inbound` to `AGENT_INBOUND_URL`; the agent appends
  `WHATSAPP_GATEWAY_SEND_PATH` (default `/send`) to `WHATSAPP_GATEWAY_URL`.
- The **same token value must be set on both boxes** for each hop (webhook token on both; gateway
  token on both). Direction is the common trip-up: **webhook** = inbound auth, **gateway** = reply
  auth.

### Peering inventory (current)

| Peering name | Attached to (source) | Peer (target) | Hostname (from source) | Must proxy to | Carries |
|---|---|---|---|---|---|
| `wagw1-to-agent` | `wagw-1` | `spike-rust-agent` | `https://wagw1-to-agent.int.exe.xyz` | agent **:8000** | inbound forwards |
| `agent-to-wagw1` | `spike-rust-agent` | `wagw-1` | `https://agent-to-wagw1.int.exe.xyz` | shim **:8080** | reply `/send` |

> **Port-mapping caveat.** A peering targets a *specific port* on the peer. `agent-to-wagw1` MUST
> land on the **shim's :8080**, not GOWA's :3000 — otherwise replies hit the wrong service. Confirm
> from the agent box: `curl -sS https://agent-to-wagw1.int.exe.xyz/livez` → expect `200 ok` (that is
> a shim route; GOWA has no `/livez`).

## Scaling out — more boxes

### A. One number → several agents (channel routing, one shim box)

`wagw-shimmy` can route each WhatsApp **group** to a different downstream **channel** (= a distinct
agent endpoint). See [`AGENT-WHATSAPP-CONTRACT.md`](AGENT-WHATSAPP-CONTRACT.md) §R3 and `src/channel.rs`.
Every channel is just *another inbound hop to another agent box* — and every agent box still replies
back to the **same shim** (replies route to WhatsApp by `chat_id`, so there is only ever one `/send`
edge per number).

For a channel label `support` pointing at agent box `agent-support`:

```
                         ┌────────────▶ wagw1-to-support  → agent-support:8000   (inbound, channel=support)
  group A ─┐             │              support-to-wagw1   → wagw-1:8080          (reply)
  group B ─┤  wagw-1     │
  group C ─┘  (shim) ────┼────────────▶ wagw1-to-agent    → spike-rust-agent:8000 (inbound, channel=default)
                         │              agent-to-wagw1     → wagw-1:8080          (reply)
                         └─ chat_id-based reply routing converges on wagw-1's single /send
```

Per extra channel `L` you add:

1. **Two peerings** (same pattern as above):
   - `wagw1-to-<L>` — attached to `wagw-1`, peer = the agent box, → its `:8000` (inbound).
   - `<L>-to-wagw1` — attached to the agent box, peer = `wagw-1`, → shim `:8080` (reply).
2. **Shim env** (`/etc/wagw-shimmy.env`):
   - add `L` to `WA_CHANNELS` (comma list),
   - `WA_CHANNEL_<L>_URL=https://wagw1-to-<L>.int.exe.xyz` (`/whatsapp/inbound` is appended),
   - `WA_CHANNEL_<L>_TOKEN=<webhook token for that agent>` (defaults to `WHATSAPP_WEBHOOK_TOKEN` if
     unset),
   - map the groups: `WA_GROUP_CHANNELS=<jidA>:<L>,<jidB>:<L>` (mapped JIDs are auto-admitted under
     `WA_GROUP_POLICY=allowlist`).
3. **Agent box env** (`/etc/spike-rust-agent.env` on `agent-<L>`):
   - `AGENT_CHANNELS` includes `whatsapp`,
   - `WHATSAPP_GATEWAY_URL=https://<L>-to-wagw1.int.exe.xyz`, `WHATSAPP_GATEWAY_SEND_PATH=/send`,
   - `WHATSAPP_WEBHOOK_TOKEN` = the channel's webhook token (matches `WA_CHANNEL_<L>_TOKEN`),
   - `WHATSAPP_GATEWAY_TOKEN` = the shim's `WHATSAPP_GATEWAY_TOKEN` (the reply edge is shared, so this
     stays the *same value* across all agent boxes that reply to this shim).

> Note: today there is **one** outbound `/send` token (`WHATSAPP_GATEWAY_TOKEN`) shared by all
> channels' agents — per-channel reply tokens are explicitly out of scope (see the contract's
> "Out of scope"). Unmapped groups and all DMs use the implicit `default` channel = `AGENT_INBOUND_URL`.

### B. Several numbers (more shim boxes)

Each WhatsApp number is its own shim box (`wagw-2`, `wagw-3`, …) with its own GOWA session and its
own pair(s) of peerings to whichever agent box(es) it uses. Nothing is shared between numbers at the
network layer — replicate section A per shim box. Naming convention scales as `<shimbox>-to-<target>`
/ `<target>-to-<shimbox>` so the peering name always reads "from → to".

## Adding a connection — checklist

For a new shim-box ⇄ agent-box link:

- [ ] Peering `<shim>-to-<agent>` attached to the **shim** box, peer = agent, → agent `:8000`.
- [ ] Peering `<agent>-to-<shim>` attached to the **agent** box, peer = shim, → shim `:8080`.
- [ ] Shim env: `AGENT_INBOUND_URL` (or `WA_CHANNEL_<L>_URL`) = `https://<shim>-to-<agent>.int.exe.xyz`.
- [ ] Agent env: `WHATSAPP_GATEWAY_URL` = `https://<agent>-to-<shim>.int.exe.xyz`, `AGENT_CHANNELS`
      includes `whatsapp`.
- [ ] Tokens match on both boxes for each hop (webhook + gateway).
- [ ] Verify (below). Only after green: turn off the shim's debug-sink for that box.

## Verification (run on the named box)

```sh
# On the AGENT box — inbound route mounted (401 = mounted+auth, NOT 404):
curl -sS -o /dev/null -w '%{http_code}\n' -X POST http://127.0.0.1:8000/whatsapp/inbound
# On the AGENT box — reply peering lands on the SHIM (200 ok), not GOWA:
curl -sS https://<agent>-to-<shim>.int.exe.xyz/livez ; echo

# On the SHIM box — inbound peering reaches the agent's health (200):
curl -sS -o /dev/null -w '%{http_code}\n' https://<shim>-to-<agent>.int.exe.xyz/health
# On the SHIM box — shim itself ready (probes GOWA + optionally the agent):
curl -sS http://127.0.0.1:8080/readyz ; echo
```

End-to-end: message the allowlisted group → `journalctl -u wagw-shimmy -f` (shim: `channel=…`,
forward `2xx`) and `make exe-logs` on the agent (whatsapp turn → `send_text`).

## Security notes

- `.int.exe.xyz` peering hostnames are reachable **only from the attached VM** — they are not a public
  surface. The agent box is additionally **public on :8000** (`spike-rust-agent.exe.xyz`); its
  `/whatsapp/inbound` is protected solely by `WHATSAPP_WEBHOOK_TOKEN`, so that token is the real
  perimeter — keep it long/random (currently 64 chars) and never log it.
- The shim's `:8080` is **not** intended to be public; it is reached only via the `*-to-wagw1`
  peering. Keep `SHIM_BIND` as-is and do not `share set-public` it.
- Both tokens live in `0600 root` env files (`/etc/wagw-shimmy.env`, `/etc/spike-rust-agent.env`).
  When copying a token between boxes, move it via a `0600` temp file and never echo the value.
