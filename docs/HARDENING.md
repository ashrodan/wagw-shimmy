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

## P0 — before first live tenant

- [ ] **Agent-forward delivery gap.** We ack GOWA *then* forward async — but if the agent forward fails (agent down, 5xx) the message is **silently lost** (GOWA won't retry; we already 200'd). Add bounded retry-with-backoff on `agent.forward`, and on final failure either (a) don't insert into dedup so a GOWA re-delivery gets another chance, or (b) write a dead-letter line. Today the dedup insert happens *before* the spawn, so a failed forward is gone. **This is the single most important reliability fix.**
- [ ] **Deep `/healthz`.** Currently returns `{status:ok}` unconditionally. Make it probe GOWA reachability (and optionally agent), so `fleetctl status` / a monitor catches "shim up but GOWA socket dead". Keep a shallow `/livez` for the process and a deep `/readyz` for dependencies.
- [ ] **GOWA binary checksum pin.** `provision.sh` downloads `GOWA_BINARY_URL` with no integrity check — supply-chain gap. Verify a pinned `sha256` (published alongside the CI artifact) before `install`.
- [ ] **systemd `LoadCredential=`.** Move secrets off `EnvironmentFile` (readable via the unit's environment / `/proc`) to `LoadCredential=` so they're only in the service's credential store. Plan already flags this as preferred "where feasible".
- [ ] **Restore drill, executed.** §3.6 of TESTING.md is currently a checklist item, not a proven capability. Restore a real snapshot into a scratch box and confirm the session loads without re-pairing — once, before relying on backups.

## P1 — before scaling the fleet

- [ ] **Bounded forward concurrency.** Inbound spawns an unbounded `tokio::spawn` per message; a burst (or a slow agent) can pile up tasks. Gate with a `Semaphore` (e.g. N in-flight) and shed/queue beyond it.
- [ ] **`sd_notify` readiness + `WatchdogSec`.** Emit the systemd ready signal once bound, and ping the watchdog from the main loop so a hung process is restarted. Units already reserve `WatchdogSec=`.
- [ ] **Metrics.** Add a `/metrics` (Prometheus) surface on the tailnet interface: inbound count, dedup drops, policy drops by reason, rate-limit hits, forward failures, GOWA 4xx/5xx, send latency. Today the only signal is log-grep in `fleetctl status`.
- [ ] **Dependency + supply-chain CI.** Add `cargo audit` / `cargo deny` (advisories + licenses) and pin GitHub Actions to commit SHAs, not tags.
- [ ] **`systemd-analyze security` pass.** Run it against the three units, drive the exposure score down (add `SystemCallFilter=@system-service`, `RestrictAddressFamilies=AF_INET AF_UNIX`, `CapabilityBoundingSet=`, `IPAddressDeny`/`Allow` to localhost+tailnet).
- [ ] **Alerting on staleness.** `fleetctl status` is pull-only. Add a cron/monitor that pages on unit-down or last-inbound older than a threshold (Tailscale admin "last seen" covers box liveness for free).
- [ ] **Structured JSON logs + rotation.** Switch tracing to JSON for machine parsing; cap journald size per unit so a chatty tenant can't fill the disk.

## P2 — opportunistic / demand-driven

- [ ] **Persist dedup + sent-id caches.** Both are in-memory: a shim restart loses the dedup window (a GOWA re-delivery just after restart could double-fire) and the sent-id cache (reply-to-bot detection blind until the bot next sends). Persist to a small sled/SQLite if either becomes a real problem.
- [ ] **True @-mention.** "Mention" is reply-to-bot by design (no GOWA patch). If a literal `@mention` summon becomes a hard need, carry a thin `vendor/gowa` patch surfacing `contextInfo.mentionedJid` and extend `model.rs`/`policy.rs`. Tracked as a deferred decision.
- [ ] **Media mapping (P1.5).** Inbound media → `media_urls`; outbound `/send/image|file|video|audio|sticker`. Coordinated agent-contract extension. See TESTING.md §4.
- [ ] **Outbound presence / receipts.** Typing indicators + read receipts passthrough for a more natural UX.
- [ ] **`tgw-shimmy` sibling.** Telegram off Hermes behind the same shim contract — explicitly out of scope now, parked here.
- [ ] **wa-rs swap readiness.** When `whatsapp-rust` reaches whatsmeow parity, a pure-Rust gateway could replace GOWA behind the same shim contract; keep the GOWA client boundary clean so the swap is local to `gowa.rs`.

## ToS / account safety (ongoing)

- [ ] Keep WA accounts disposable and segmented per tenant (one per box, already enforced by isolation).
- [ ] Tune `WA_SEND_RATE_PER_MIN` conservatively; consider per-chat as well as per-tenant caps if spray patterns emerge.
- [ ] Monitor for bans; have a re-pair / new-number runbook ready (PAIRING.md covers re-pair).
