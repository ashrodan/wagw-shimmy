# Hardening backlog

Prioritised backlog of reliability/security/operability work beyond the shipped P0/P1 shim. Tiers:

- **P0** — do before any tenant carries real traffic.
- **P1** — do before scaling past one or two tenants.
- **P2** — nice-to-have / depends on demand.
- **done** — already landed (kept here for the audit trail).

Status: `[x]` done · `[ ]` open. Each item names the *why*, not just the *what*.

## Done

- [x] **Constant-time bearer comparison** — `/send` bearer + (future) any token check go through `subtle::ct_eq`, so auth doesn't leak how many leading bytes matched via timing. (`server.rs::constant_time_eq`)
- [x] **Request body-size cap** — 256 KiB `DefaultBodyLimit` on all routes; a loopback peer can't force us to buffer arbitrary memory. 1 MiB body → 413, asserted in e2e.
- [x] **Raw-bytes HMAC verify** — inbound verified over the exact bytes with constant-time compare before any deserialisation.
- [x] **CI on Node-24 actions + Go cache** — `checkout@v6`/`setup-go@v6`/`upload-artifact@v7`; cache keyed on the submodule `go.sum`.
- [x] **Durable forward queue + bounded worker (closes the agent-forward delivery gap *and* the unbounded-concurrency P1).** Accepted inbound is written to `<SHIM_QUEUE_DIR>/pending/<hex(id)>.json` (atomic tmp+rename) *before* GOWA is acked; a `Semaphore`-bounded worker forwards with backoff retries, deletes on 2xx, and dead-letters to `dead/` on exhaustion. Drains on startup; survives agent outage + shim restart. (`forward.rs`)
- [x] **`/livez` + `/readyz`.** `/livez` (and the `/healthz` alias) is static process liveness; `/readyz` probes GOWA `/devices` (and optionally the agent's `/health` via `SHIM_READYZ_PROBE_AGENT`) and returns 200/503. `fleetctl status` shows a READY column. (`server.rs`, `gowa.rs::ping`)
- [x] **GOWA binary checksum pin.** CI publishes `gowa-<tag>-linux-amd64.sha256`; `provision.sh` requires `GOWA_BINARY_SHA256` and verifies before `install` (aborts on mismatch).
- [x] **systemd `LoadCredential=` for the shim.** The shim's four secrets load from 0600 credential files (rendered by `render-env.sh`) via `LoadCredential=` + `<NAME>_FILE`, so they never enter the shim's `/proc/<pid>/environ`. Config reads `secret()` = direct env else `<NAME>_FILE`. **Residual risk:** GOWA has no upstream file-secret support, so `/etc/gowa.env` still carries `APP_BASIC_AUTH` + `WHATSAPP_WEBHOOK_SECRET` in GOWA's environment (the unit is `NoNewPrivileges` + `ProtectSystem=strict`; a vendored GOWA patch is a deferred option — see P2).

## P0 — before first live tenant

- [ ] **Restore drill, executed.** §3.6 of TESTING.md is currently a checklist item, not a proven capability. Restore a real snapshot into a scratch box and confirm the session loads without re-pairing — once, before relying on backups. (Record in a dated `docs/acceptance-<date>.md`.)

## Cross-repo follow-on (filed, not in this repo)

- [ ] **Agent credential files (`spike-rust-agent`).** Mirror the shim's `*_FILE` + `LoadCredential=` for the agent's WhatsApp/OpenAI secrets in `agent.service`, so `WHATSAPP_*` + `OPENAI_BEARER_TOKEN` leave the agent's `/proc` environment too. Until then `render-env.sh` keeps `/etc/agent.env` inline. **Tracked as a separate PR in that repo.**

## P1 — before scaling the fleet

- [ ] **`sd_notify` readiness + `WatchdogSec`.** Emit the systemd ready signal once bound, and ping the watchdog from the main loop so a hung process is restarted. Units already reserve `WatchdogSec=`.
- [ ] **Metrics.** Add a `/metrics` (Prometheus) surface on the tailnet interface: inbound count, dedup drops, policy drops by reason, rate-limit hits, forward failures, GOWA 4xx/5xx, send latency. Today the only signal is log-grep in `fleetctl status`.
- [ ] **Dependency + supply-chain CI.** Add `cargo audit` / `cargo deny` (advisories + licenses) and pin GitHub Actions to commit SHAs, not tags.
- [ ] **`systemd-analyze security` pass.** Run it against the three units, drive the exposure score down (add `SystemCallFilter=@system-service`, `RestrictAddressFamilies=AF_INET AF_UNIX`, `CapabilityBoundingSet=`, `IPAddressDeny`/`Allow` to localhost+tailnet).
- [ ] **Alerting on staleness.** `fleetctl status` is pull-only. Add a cron/monitor that pages on unit-down or last-inbound older than a threshold (Tailscale admin "last seen" covers box liveness for free).
- [ ] **Structured JSON logs + rotation.** Switch tracing to JSON for machine parsing; cap journald size per unit so a chatty tenant can't fill the disk.
- [ ] **Make `GOWA_DEVICE_ID` optional.** It's required at boot but only known *after* pairing, forcing the awkward "pair before starting the shim" order (see FIRST-NUMBER.md). In single-device GOWA an omitted `X-Device-Id` resolves to the default device — so default it to empty and skip the header when unset, letting the shim start before the first pairing.

## P2 — opportunistic / demand-driven

- [ ] **Persist dedup + sent-id caches.** Both are in-memory: a shim restart loses the dedup window (a GOWA re-delivery just after restart could double-fire) and the sent-id cache (reply-to-bot detection blind until the bot next sends). Persist to a small sled/SQLite if either becomes a real problem.
- [ ] **True @-mention.** "Mention" is reply-to-bot by design (no GOWA patch). If a literal `@mention` summon becomes a hard need, carry a thin `vendor/gowa` patch surfacing `contextInfo.mentionedJid` and extend `model.rs`/`policy.rs`. Tracked as a deferred decision.
- [ ] **Media mapping (P1.5).** Inbound media → `media_urls`; outbound `/send/image|file|video|audio|sticker`. Coordinated agent-contract extension. See TESTING.md §4.
- [ ] **Outbound presence / receipts.** Typing indicators shipped: `POST /send/chat-presence` forwards `{phone, action}` to GOWA (bearer-gated, off the send budget; see `AGENT-WHATSAPP-CONTRACT.md`). Read-receipt passthrough still outstanding.
- [ ] **`tgw-shimmy` sibling.** Telegram off Hermes behind the same shim contract — explicitly out of scope now, parked here.
- [ ] **wa-rs swap readiness.** When `whatsapp-rust` reaches whatsmeow parity, a pure-Rust gateway could replace GOWA behind the same shim contract; keep the GOWA client boundary clean so the swap is local to `gowa.rs`.

## ToS / account safety (ongoing)

- [ ] Keep WA accounts disposable and segmented per tenant (one per box, already enforced by isolation).
- [ ] Tune `WA_SEND_RATE_PER_MIN` conservatively; consider per-chat as well as per-tenant caps if spray patterns emerge.
- [ ] Monitor for bans; have a re-pair / new-number runbook ready (PAIRING.md covers re-pair).
