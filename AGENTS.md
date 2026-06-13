# AGENTS.md

Guidance for coding agents (and humans) working in this repository. `CLAUDE.md` is a symlink here.

## What this is

`wagw-shimmy` is the **WhatsApp Gateway Shim**: a thin Rust/axum adapter bridging **GOWA**
(`go-whatsapp-web-multidevice`, built on whatsmeow, pinned v8.7.0) to the **`spike-rust-agent`**
inbound/outbound contract, applying per-tenant policy. It is the only new code between GOWA and the
agent; GOWA runs unmodified (or via a thin vendored patch series). See `README.md` for the payload
schema, HMAC scheme, and DM/group sequence diagrams, and `deploy/README.md` for the fleet.

## Build, test, lint

```sh
cargo build
cargo test                          # 25 unit + 9 e2e — no network, no WA account, no API spend
cargo clippy --all-targets -- -D warnings
cargo fmt --all --check
```

Always run clippy + test before considering a change done. Tests must never require a real WhatsApp
account or external network: the e2e suite stands up in-process mock GOWA + agent servers.

## The one invariant

The shim forwards GOWA's `payload.chat_id` (the **conversation** JID) inbound, and the agent echoes
that same `chat_id` back on `/send`. That round-trip is what makes DM **and** group replies land in
the right place with no special-casing. `from` (participant JID) is internal-only, for DM
allowlisting. Confusing `chat_id` with `from` is the "answered in DM instead of the group" bug — the
e2e test `reply_to_bot_summons_in_require_mention_group` guards it.

## Module map (`src/`)

- `main.rs` — entry: tracing init, config load (fail fast), serve with SIGTERM drain.
- `lib.rs` — module exports + `AppState`/`build_router` re-export (so tests drive the real router).
- `config.rs` — env parsing + startup validation; `Config`, `PolicyConfig`, JID normalisation.
- `error.rs` — `HttpError` (bad_request / unauthorized / rate_limited / upstream); never logs secrets.
- `model.rs` — GOWA webhook wire types + the internal `Inbound`; structural drops.
- `gowa.rs` — GOWA client (`/send/message`) + inbound HMAC verify over raw bytes.
- `agent.rs` — agent client (`POST /whatsapp/inbound` + bearer); forwards the 4-field contract only.
- `policy.rs` — **pure**, unit-tested admission policy (DM/group allowlist + require-mention).
- `dedup.rs` — bounded-TTL `TtlSet` (drops GOWA re-deliveries).
- `sent_ids.rs` — bounded-TTL cache of the bot's own sent ids → reply-to-bot mention detection.
- `ratelimit.rs` — per-tenant outbound send limiter (ToS protection).
- `server.rs` — axum wiring + the two mapping handlers (ack-fast inbound, bearer-checked outbound).

## Conventions

Mirrors `spike-rust-agent`: `edition = "2024"`, axum 0.8 + tokio + reqwest (rustls), small explicit
error enum, env helpers that treat all-whitespace as absent. Keep policy pure (no IO/clock) so it
stays trivially testable. Never put a secret in an `HttpError`, a log line, or a `Debug` impl.
