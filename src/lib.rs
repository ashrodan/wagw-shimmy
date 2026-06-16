//! `wagw-shimmy` — the WhatsApp Gateway Shim.
//!
//! A thin Rust/axum adapter that bridges **GOWA** (`go-whatsapp-web-multidevice`, built on
//! whatsmeow) to the **spike-rust-agent** inbound/outbound contract, applying per-tenant policy:
//!
//! ```text
//! WhatsApp ⟷ GOWA :3000  ──webhook──▶  wagw-shimmy :8080  ──/whatsapp/inbound──▶  agent :3001
//!                       ◀──/send/message──               ◀──────/send──────────
//! ```
//!
//! The single most important correctness invariant: the shim forwards GOWA's `payload.chat_id`
//! (the *conversation* JID) inbound, and the agent echoes that same `chat_id` back on `/send`. That
//! round-trip is what makes DM and group replies land in the right conversation with no
//! special-casing — `@g.us` = group, `@s.whatsapp.net` = DM.
//!
//! Modules are exposed for integration tests; the binary entry point is `src/main.rs`.

pub mod agent;
pub mod channel;
pub mod config;
pub mod dedup;
pub mod error;
pub mod forward;
pub mod gowa;
pub mod model;
pub mod policy;
pub mod ratelimit;
pub mod sent_ids;
pub mod server;

pub use server::{AppState, build_router};
