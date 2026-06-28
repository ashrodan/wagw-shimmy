# Testing checklist

Three tiers: **automated** (runs in CI, no WhatsApp/network), **local E2E** (real GOWA + a throwaway
WA account, before a tenant goes live), and **acceptance** (on a provisioned fleet box). Acceptance
mirrors the plan's verification section; passing steps 2 + 3 + 4 is the bar for "a tenant is live".

Legend: ✅ automated & passing · ⬜ manual, do before go-live · 🔁 recurring (per release / per tenant)

## 1. Automated (CI gate — `cargo fmt`/`clippy -D`/`test` + GOWA build)

These run on every push/PR and must stay green. 36 unit + 16 e2e.

- ✅ **HMAC verify** — sign/verify roundtrip; reject wrong secret, tampered body, missing/!hex/no-prefix header (`gowa.rs`).
- ✅ **Inbound mapping** — group/DM payload → internal model; `from` fallback to `chat_id` for DMs (`model.rs`).
- ✅ **Structural drops** — non-message event, `is_from_me`, `status@broadcast`, `@newsletter` (`model.rs`).
- ✅ **Policy matrix** — DM open/allowlist/off; group off/allowlist/open; require-mention; free-response bypass (`policy.rs`).
- ✅ **Dedup** — `insert_new` once-true-then-false; TTL expiry; capacity eviction (`dedup.rs`).
- ✅ **Reply-to-bot** — sent-id recorded → `is_reply_to_bot` true; unknown/empty/None false (`sent_ids.rs`).
- ✅ **Rate limit** — burst then block within the minute (`ratelimit.rs`).
- ✅ **Auth (constant-time)** — `bearer_matches` exact-token only; `constant_time_eq` length + content (`server.rs`).
- ✅ **E2E — inbound** DM forwarded with the 4-field contract only (no internal fields leak); chat_id verbatim.
- ✅ **E2E — bad signature** → 401, never forwarded.
- ✅ **E2E — dedup** same id twice → agent invoked once.
- ✅ **E2E — from_me echo** dropped.
- ✅ **E2E — plain group msg** dropped under require-mention.
- ✅ **E2E — ack-fast** webhook returns <500 ms even with a 2 s agent (proves ack ≠ blocked on agent).
- ✅ **E2E — reply-to-bot** summons the bot in a require-mention group; **reply lands in the group, not the sender's DM** (the chat_id-vs-from regression guard).
- ✅ **E2E — /send bearer** required; unauthenticated send never reaches GOWA.
- ✅ **E2E — body limit** 1 MiB body → 413, not forwarded.
- ✅ **E2E — livez/readyz** `/livez` 200 with no deps; `/readyz` 200 with healthy mock GOWA, 503 when GOWA is down.
- ✅ **Durable forward queue** — hex filename + traversal safety, idempotent enqueue, JSON round-trip, backoff growth/clamp (`forward.rs`).
- ✅ **E2E — durable forward** retry-then-succeed clears `pending/`; agent-down dead-letters to `dead/`; a pre-seeded file drains on startup.
- ✅ **Secret resolution** — direct env wins over `<NAME>_FILE`; file read+trimmed when env absent; empty/missing → none; required-secret error names the variable, not the value (`config.rs`).
- ✅ **GOWA artifact build** — vendored v8.7.0 submodule compiles clean in CI (proves the pin resolves & builds).
- ✅ **Shell (deploy/tests)** — checksum verify accepts good/rejects bad; fleetctl invokes no `exe.dev new`/`rm` + enforces host validation; render-env writes 0600 credential files and keeps secrets out of the env file.

## 2. Local E2E — real GOWA + throwaway WA account (P2, before first tenant)

Run the three processes locally (GOWA :3000, shim :8080, agent :3001) against a **disposable** WA
number. Do the mock pass first, then the real account.

- ⬜ Boot all three; `curl localhost:8080/healthz` → ok; GOWA shows no session yet.
- ⬜ Pair (QR or pairing-code, see [PAIRING.md](PAIRING.md)); GOWA shows a linked session.
- ⬜ **DM round-trip** — from an allowlisted number, DM the bot → agent replies in the same DM.
- ⬜ **DM policy** — from a non-allowlisted number → silently dropped at the shim (check shim log "dropped by policy").
- ⬜ **Group round-trip (gated)** — add bot to an allowlisted group, `WA_REQUIRE_MENTION=true`; a plain message → silence; reply to the bot's message → it answers **in the group**.
- ⬜ **Group free-response** — add the JID to `WA_FREE_RESPONSE_CHATS` → bot answers without a reply-to.
- ⬜ **Dedup under latency** — make the agent slow (>10 s); confirm GOWA does not produce a duplicate reply.
- ⬜ **Outbound errors** — stop GOWA mid-send → shim returns 502 `upstream`; send to a bad JID → 400 `bad_request`.
- ⬜ **Rate limit** — exceed `WA_SEND_RATE_PER_MIN` → shim returns 429, no GOWA call.

## 3. Acceptance — on a provisioned fleet box (per tenant, 🔁)

Mirrors the plan's verification. **Acceptance = steps 2 + 3 + 4 pass.**

1. ⬜ **Health** — `systemctl is-active gowa wagw-shimmy agent` → all `active`; GOWA paired; shim `/healthz` ok.
2. ⬜ **DM round-trip** — allowlisted number DMs the account → agent replies in the same DM (proves `WA_DM_POLICY` on `from` + chat_id echo).
3. ⬜ **Group round-trip** — allowlisted group; plain msg stays silent under require-mention; reply-to-bot gets an answer **in the group** (assert it is NOT the sender's DM); add to `WA_FREE_RESPONSE_CHATS` → answers without mention.
4. ⬜ **Dedup / timeout** — replay the same webhook id twice → agent invoked once; slow agent (>10 s) → no duplicate reply.
5. ⬜ **Policy / isolation** — non-allowlisted sender/group dropped at the shim; `is_from_me` ignored; this box's data dir + WA account fully separate.
6. ⬜ **Backup + restore** — run `backup.sh`; `restic snapshots` shows the tenant prefix incl. the whatsmeow store; **restore into a scratch box and confirm the session loads without re-pairing** (reboot ≠ re-pair, proven by restore, not snapshot existence). Execute + record this in a dated `docs/acceptance-<date>.md` (scaffold: [`docs/acceptance-2026-06-14.md`](acceptance-2026-06-14.md)) — it is the readiness sign-off, not just a checkbox.

## 4. Media (P1.5 — when implemented)

- ⬜ Inbound image/audio/document → caption surfaces in `body`; **with `WHATSAPP_AUTO_DOWNLOAD_MEDIA=true` (required)**, the file is proxied to the agent as `media[{type,url,…}]` and `GET /media/<token>` (with the `/send` bearer) streams the bytes. With auto-download **off**, media silently drops (CDN URL the shim can't decrypt) — confirm the env is `true`.
- ⬜ Golden fixtures per media type added to `tests/fixtures` and asserted.
- ⬜ Outbound media send (`/send/image`, `/send/file`, …) mapped and bearer-gated.

## 4b. Reactions (emoji)

- ⬜ Inbound: react to a message in a DM → agent receives a `type:"reaction"` inbound with the emoji + `reacted_message_id`. Requires `WHATSAPP_WEBHOOK_EVENTS` to include `message.reaction`.
- ⬜ Group policy: react to the bot's own message in a require-mention group → forwarded; react to someone else's → dropped.
- ⬜ Outbound: `POST /send/reaction {chat_id, message_id, emoji}` → the emoji appears on the message in WhatsApp; empty `emoji` removes it. Bearer-gated + rate-limited.

## Release ritual

Before tagging / bumping the pinned GOWA tag:

- 🔁 `cargo fmt --all --check && cargo clippy --all-targets -- -D warnings && cargo test` (or just push — CI runs it).
- 🔁 If bumping GOWA: refresh golden fixtures against the new payload shapes; rebuild the CI artifact; re-run the diff against `vendor/gowa/docs/webhook-payload.md`.
- 🔁 Re-run §3 acceptance on a canary tenant before rolling the fleet.
