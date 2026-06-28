# Setting up the first number (manual runbook)

Do the **first** tenant by hand, not via `fleetctl`. Reasons: the fleet automation
(`provision.sh`/`fleetctl`) has never run against a real box or a real WhatsApp account yet, and
pairing is interactive anyway. Setting one up manually proves the whole stack end-to-end and shakes
out the env wiring, so you trust the automation before pointing it at boxes.

> Use a **disposable** WhatsApp number for the first run (unofficial whatsmeow client → ToS risk).
> Everything below binds to `127.0.0.1` — nothing is exposed publicly.

## 0. Where to run it

Any Linux box works (an exe.dev box, a cheap VPS, or a local VM). You need: the **GOWA v8.7.0 binary**
for the box's platform (built from `vendor/gowa` — see `vendor/README.md`, or grab the CI artifact),
a Rust toolchain to build the shim + agent, and outbound internet for WhatsApp + the LLM.

```sh
# On the box — fetch this repo + the agent sibling
git clone --recursive https://github.com/ashrodan/wagw-shimmy
git clone https://github.com/ashrodan/spike-rust-agent
cargo build --release --manifest-path wagw-shimmy/Cargo.toml         # → wagw-shimmy/target/release/wagw-shimmy
cargo build --release --manifest-path spike-rust-agent/Cargo.toml    # → .../spike-rust-agent
# GOWA binary (CI artifact or local build of vendor/gowa/src) → /usr/local/bin/gowa
```

## 1. Generate the shared secrets (once)

Four secrets, three of them shared between two processes. Generate and keep them somewhere safe.

```sh
GOWA_BASIC_PASS=$(openssl rand -hex 16)      # GOWA REST basic-auth password (user = "admin")
WEBHOOK_SECRET=$(openssl rand -hex 32)        # GOWA ⇄ shim  HMAC secret  (must NOT be "secret")
WEBHOOK_TOKEN=$(openssl rand -hex 32)         # shim → agent bearer
GATEWAY_TOKEN=$(openssl rand -hex 32)         # agent → shim bearer
echo "GOWA_BASIC_PASS=$GOWA_BASIC_PASS"; echo "WEBHOOK_SECRET=$WEBHOOK_SECRET"
echo "WEBHOOK_TOKEN=$WEBHOOK_TOKEN"; echo "GATEWAY_TOKEN=$GATEWAY_TOKEN"
```

The wiring that must match:

| value | set on GOWA | set on shim | set on agent |
|---|---|---|---|
| basic auth | `APP_BASIC_AUTH=admin:$PASS` | `GOWA_BASIC_AUTH=admin:$PASS` | — |
| HMAC secret | `WHATSAPP_WEBHOOK_SECRET=$WEBHOOK_SECRET` | `GOWA_WEBHOOK_SECRET=$WEBHOOK_SECRET` | — |
| inbound bearer | — | `WHATSAPP_WEBHOOK_TOKEN=$WEBHOOK_TOKEN` | `WHATSAPP_WEBHOOK_TOKEN=$WEBHOOK_TOKEN` |
| send bearer | — | `WHATSAPP_GATEWAY_TOKEN=$GATEWAY_TOKEN` | `WHATSAPP_GATEWAY_TOKEN=$GATEWAY_TOKEN` |

## 2. Lay down the three env files

Mirror these into `/etc/{gowa,wagw-shimmy,agent}.env` (or just `export` them for a foreground first
run). `GOWA_DEVICE_ID` is **left blank until after pairing** (§5).

```sh
# /etc/gowa.env
APP_PORT=3000
APP_BASIC_AUTH=admin:$GOWA_BASIC_PASS
WHATSAPP_WEBHOOK=http://127.0.0.1:8080/webhook/gowa
WHATSAPP_WEBHOOK_SECRET=$WEBHOOK_SECRET
WHATSAPP_WEBHOOK_EVENTS=message,message.reaction
WHATSAPP_AUTO_DOWNLOAD_MEDIA=true

# /etc/wagw-shimmy.env
SHIM_BIND=127.0.0.1:8080
GOWA_URL=http://127.0.0.1:3000
GOWA_BASIC_AUTH=admin:$GOWA_BASIC_PASS
GOWA_DEVICE_ID=                         # ← fill after pairing (§5)
GOWA_WEBHOOK_SECRET=$WEBHOOK_SECRET
AGENT_INBOUND_URL=http://127.0.0.1:3001
WHATSAPP_WEBHOOK_TOKEN=$WEBHOOK_TOKEN
WHATSAPP_GATEWAY_TOKEN=$GATEWAY_TOKEN
WA_DM_POLICY=allowlist
WA_DM_ALLOW=<your-own-number>           # e.g. 61400111222 — start with ONLY yourself
WA_GROUP_POLICY=off                     # turn groups on later, once DM works
WA_REQUIRE_MENTION=true
WA_SEND_RATE_PER_MIN=20
RUST_LOG=info

# /etc/agent.env
AGENT_BIND_ADDR=127.0.0.1:3001
AGENT_CHANNELS=whatsapp
WHATSAPP_GATEWAY_URL=http://127.0.0.1:8080
WHATSAPP_GATEWAY_TOKEN=$GATEWAY_TOKEN
WHATSAPP_WEBHOOK_TOKEN=$WEBHOOK_TOKEN
OPENAI_BEARER_TOKEN=<your-llm-key>      # or the agent's codex-auth vars; see spike-rust-agent
```

## 3. Start the stack (foreground for the first run)

Three terminals (or `tmux`), in order — GOWA, then shim, then agent:

```sh
# 1) GOWA  (holds the WA socket; REST :3000)
set -a; . /etc/gowa.env; set +a
gowa rest --port 3000 --db-uri "file:$PWD/whatsapp.db?_foreign_keys=on"

# 2) shim  (:8080)
set -a; . /etc/wagw-shimmy.env; set +a
./wagw-shimmy/target/release/wagw-shimmy        # NOTE: will refuse to boot until GOWA_DEVICE_ID is set?
#   → no: the shim boots fine with GOWA_DEVICE_ID blank only if you leave it set to a placeholder.
#     GOWA_DEVICE_ID is REQUIRED at boot. For the very first start, set it to "pending" then
#     correct it in §5 — or just start GOWA + pair first, then start the shim with the real JID.

# 3) agent (:3001)
set -a; . /etc/agent.env; set +a
./spike-rust-agent/target/release/spike-rust-agent serve 127.0.0.1:3001
```

> Boot order note: `GOWA_DEVICE_ID` is a **required** shim env (fail-fast). Since the JID is unknown
> until pairing, the clean sequence is: **start GOWA → pair (§4–5) → set the real JID → start the
> shim → start the agent.** Don't start the shim before you have the JID.

Sanity: `curl -s localhost:8080/healthz` → `{"status":"ok"}` (once the shim is up).

## 4. Pair the number (pairing-code, no browser)

```sh
PHONE=61400111222        # the disposable number, E.164, no +
curl -fsS -u "admin:$GOWA_BASIC_PASS" \
  "http://127.0.0.1:3000/app/login-with-code?phone=$PHONE"
# → {"results":{"pair_code":"ABCD-EFGH", ...}}
```

On that phone: **WhatsApp → Settings → Linked Devices → Link a device → "Link with phone number
instead"** → enter the code. (QR alternative + full detail: `docs/PAIRING.md`.)

## 5. Capture the JID → finish shim config

```sh
curl -fsS -u "admin:$GOWA_BASIC_PASS" http://127.0.0.1:3000/devices
# → find your device's "jid", e.g. "61400111222:23@s.whatsapp.net"
```

Put that JID into `GOWA_DEVICE_ID` in `/etc/wagw-shimmy.env`, then start (or restart) the shim. The
shim sends it as `X-Device-Id` on every send, and uses it to route.

## 6. Verify (the acceptance bar)

- **DM:** from your allowlisted number, DM the linked account → the agent replies **in the same DM**.
- **Wrong sender:** from a non-allowlisted number → silence; shim log shows `dropped by policy`.
- **Group (optional, after DM works):** set `WA_GROUP_POLICY=allowlist`, add the group JID to
  `WA_GROUP_ALLOW`, restart the shim. Plain group message → silence (require-mention); **reply to the
  bot's message** → it answers **in the group** (not your DM).
- **No duplicates:** if the agent is slow (>10 s), you still get exactly one reply.

Full matrix: `docs/TESTING.md` §3.

## 7. Take the first backup immediately

The whatsmeow session now lives in `whatsapp.db`. Before anything else can crash and lose it:

```sh
# minimal consistent snapshot (full per-tenant restic→R2 backup is deploy/backup.sh)
sqlite3 whatsapp.db "PRAGMA wal_checkpoint(TRUNCATE);"
sqlite3 whatsapp.db ".backup 'whatsapp-firstpair.db'"     # copy this off-box
```

## 8. Promote (later)

Once the manual run is solid: move the env files to `/etc/*.env` (0600), install the three systemd
units (`deploy/*.service`), and from then on use `fleetctl provision` / `fleetctl pair` for subsequent
numbers. The manual run is the reference the automation should reproduce.

> **VM creation stays manual.** `fleetctl` never creates or destroys boxes — that is an
> operator/infra prerequisite. Create the VM (and record its `box:`/`magicdns:` in
> `deploy/tenants/<id>.yaml`) first; `fleetctl provision <id>` only stands the app stack up on an
> already-declared, reachable host.

## Gotchas seen / to watch

- `GOWA_DEVICE_ID` is required at shim boot but only known post-pair → pair before starting the shim.
- `GOWA_WEBHOOK_SECRET` must not be `secret` (boot refuses it) and must equal GOWA's `WHATSAPP_WEBHOOK_SECRET`.
- The agent listens on **:3001** (GOWA owns :3000). Don't collide them.
- Allowlist yourself only (`WA_DM_ALLOW`) for the first run; widen later.
