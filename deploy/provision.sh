#!/usr/bin/env bash
# provision.sh — stand up the 3-process stack on a fresh exe.dev box. IDEMPOTENT: re-running must
# not break a live tenant (it re-renders env, reinstalls binaries/units, and restarts cleanly).
#
# Runs ON the box. Expects:
#   - this repo checked out (or rsync'd) to the box,
#   - secret values present in the environment (pushed by `fleetctl add` before invoking this),
#   - the prebuilt GOWA binary fetched from the CI artifact store (GOWA_BINARY_URL).
#
# Usage: provision.sh <tenant-id>
set -euo pipefail

TENANT="${1:?usage: provision.sh <tenant-id>}"
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO="$(cd "$HERE/.." && pwd)"

log() { printf '\n[provision:%s] %s\n' "$TENANT" "$*"; }
die() { printf '\n[provision:%s] ERROR: %s\n' "$TENANT" "$*" >&2; exit 1; }

# 0. Service user + per-tenant data dirs (idempotent).
log "ensuring service user + data dirs"
id wagw >/dev/null 2>&1 || sudo useradd --system --create-home --home-dir /var/lib/wagw --shell /usr/sbin/nologin wagw
sudo install -d -o wagw -g wagw -m 0750 /var/lib/wagw /var/lib/wagw/gowa /var/lib/wagw/shim /var/lib/wagw/agent
# Durable forward queue (the shim also self-creates these at boot, but pre-make them so the dirs
# exist with the right owner before the unit starts).
sudo install -d -o wagw -g wagw -m 0750 /var/lib/wagw/shim/queue /var/lib/wagw/shim/queue/pending /var/lib/wagw/shim/queue/dead

# 1. Join the tailnet early (so the operator can reach the box even if later steps wobble).
log "joining tailnet"
MAGICDNS="$(grep -E '^magicdns:' "$REPO/deploy/tenants/${TENANT}.yaml" | awk '{print $2}')"
TS_AUTHKEY="${TAILSCALE_AUTHKEY:?TAILSCALE_AUTHKEY must be set}" \
  "$HERE/tailscale-up.sh" "${MAGICDNS:-wa-$TENANT}"

# 2. Rust toolchain (idempotent) — used to build the shim + agent from source on the box.
if ! command -v cargo >/dev/null 2>&1; then
  log "installing Rust toolchain"
  curl --proto '=https' --tlsv1.2 -fsSL https://sh.rustup.rs | sh -s -- -y
  # shellcheck disable=SC1091
  . "$HOME/.cargo/env"
fi
export PATH="$HOME/.cargo/bin:$PATH"

# 3. Install the PREBUILT GOWA binary (no Go toolchain on the box).
#    Integrity is verified against GOWA_BINARY_SHA256 (the CI gowa-artifact job publishes a matching
#    .sha256; the official GOWA release path also ships a checksums.txt — what the live wagw-1 install
#    used). A mismatch aborts before install: a wrong/compromised/tag-drifted artifact never lands.
log "installing prebuilt GOWA binary"
: "${GOWA_BINARY_URL:?GOWA_BINARY_URL must point at the CI-built, pinned (v8.7.0) GOWA artifact}"
: "${GOWA_BINARY_SHA256:?GOWA_BINARY_SHA256 must be the expected sha256 of the GOWA artifact}"
tmp="$(mktemp)"
curl -fsSL "$GOWA_BINARY_URL" -o "$tmp"
if ! printf '%s  %s\n' "$GOWA_BINARY_SHA256" "$tmp" | sha256sum -c - >/dev/null 2>&1; then
  rm -f "$tmp"
  die "GOWA binary checksum mismatch — refusing to install (expected $GOWA_BINARY_SHA256)"
fi
sudo install -m 0755 "$tmp" /usr/local/bin/gowa
rm -f "$tmp"

# 4. Build + install the shim. When the tenant enables voice-note transcription, build with
#    `--features transcribe` (pulls in whisper.cpp, which needs cmake + a C++ toolchain) and
#    provision the multilingual ggml model to /var/lib/wagw/shim/models/. Otherwise build the thin
#    default (no model, no C++), exactly as before.
# transcribe.* live under the nested `transcribe:` block; pull each nested key.
tk() { sed -n '/^transcribe:/,/^[a-z_]/p' "$REPO/deploy/tenants/${TENANT}.yaml" | grep -E "^[[:space:]]+$1:" | head -1 | sed -E "s/^[^:]*:[[:space:]]*//; s/[\"']//g"; }
TRANSCRIBE_ENABLED="$(tk enabled)"
WHISPER_MODEL="$(tk model)"
WHISPER_MODEL_URL="$(tk model_url)"
TRANSCRIBE_LANG="$(tk language)"

if [ "$TRANSCRIBE_ENABLED" = "true" ]; then
  : "${WHISPER_MODEL:?transcribe.enabled but transcribe.model (ggml filename) missing in tenant yaml}"
  log "installing transcribe build deps (cmake + clang/libclang for whisper.cpp bindgen; libopus+pkg-config for the opus crate)"
  sudo apt-get update -y && sudo apt-get install -y cmake clang libclang-dev pkg-config libopus-dev
  log "building wagw-shimmy (--features transcribe)"
  ( cd "$REPO" && cargo build --release --features transcribe --bin wagw-shimmy )
  # Provision the multilingual ggml model (idempotent: skip if already present).
  MODEL_DIR=/var/lib/wagw/shim/models
  sudo install -d -o wagw -g wagw -m 0750 "$MODEL_DIR"
  if [ ! -s "$MODEL_DIR/$WHISPER_MODEL" ]; then
    : "${WHISPER_MODEL_URL:?transcribe.enabled but transcribe.model_url missing and model not on box}"
    log "fetching whisper model $WHISPER_MODEL"
    tmpm="$(mktemp)"; curl -fsSL "$WHISPER_MODEL_URL" -o "$tmpm"
    sudo install -o wagw -g wagw -m 0640 "$tmpm" "$MODEL_DIR/$WHISPER_MODEL"; rm -f "$tmpm"
  fi
else
  log "building wagw-shimmy"
  ( cd "$REPO" && cargo build --release --bin wagw-shimmy )
fi
sudo install -m 0755 "$REPO/target/release/wagw-shimmy" /usr/local/bin/wagw-shimmy

# 5. Build + install the agent (sibling checkout expected at ../spike-rust-agent).
AGENT_REPO="${AGENT_REPO:-$REPO/../spike-rust-agent}"
log "building spike-rust-agent from ${AGENT_REPO}"
( cd "$AGENT_REPO" && cargo build --release --bin spike-rust-agent )
sudo install -m 0755 "$AGENT_REPO/target/release/spike-rust-agent" /usr/local/bin/spike-rust-agent

# 6. Render envs (0600) from the tenant yaml + secret env. Policy values are extracted here so the
#    renderer stays a dumb templater.
log "rendering /etc/*.env"
y() { grep -E "^[[:space:]]*$1:" "$REPO/deploy/tenants/${TENANT}.yaml" | head -1 | sed -E "s/^[^:]*:[[:space:]]*//; s/[\"']//g"; }
list() { sed -n "/^[[:space:]]*$1:/,/^[[:space:]]*[a-z_]*:/p" "$REPO/deploy/tenants/${TENANT}.yaml" \
           | grep -E '^[[:space:]]+-' | sed -E 's/^[[:space:]]+-[[:space:]]*//; s/[\"'\'']//g' | paste -sd, -; }
# Optional per-group channel routing → WA_CHANNELS / WA_CHANNEL_<L>_URL / WA_GROUP_CHANNELS env lines
# (empty when the yaml has no channels: block → default-only, unchanged).
mapfile -t CHANNEL_ENV < <(awk -f "$HERE/channels-env.awk" "$REPO/deploy/tenants/${TENANT}.yaml")
sudo env \
  GOWA_BASIC_AUTH="${GOWA_BASIC_AUTH:?}" \
  GOWA_WEBHOOK_SECRET="${GOWA_WEBHOOK_SECRET:?}" \
  WHATSAPP_WEBHOOK_TOKEN="${WHATSAPP_WEBHOOK_TOKEN:?}" \
  WHATSAPP_GATEWAY_TOKEN="${WHATSAPP_GATEWAY_TOKEN:?}" \
  OPENAI_BEARER_TOKEN="${OPENAI_BEARER_TOKEN:?}" \
  GOWA_DEVICE_ID="$(y device_jid)" \
  WA_DM_POLICY="$(y dm_policy)" \
  WA_DM_ALLOW="$(list dm_allow)" \
  WA_GROUP_POLICY="$(y group_policy)" \
  WA_GROUP_ALLOW="$(list group_allow)" \
  WA_REQUIRE_MENTION="$(y require_mention)" \
  WA_FREE_RESPONSE_CHATS="$(list free_response_chats)" \
  WA_SEND_RATE_PER_MIN="$(y send_rate_per_min)" \
  TRANSCRIBE_ENABLED="${TRANSCRIBE_ENABLED}" \
  WHISPER_MODEL="${WHISPER_MODEL}" \
  TRANSCRIBE_LANG="${TRANSCRIBE_LANG}" \
  "${CHANNEL_ENV[@]}" \
  bash "$HERE/render-env.sh" "$TENANT"

# 7. Install + enable the systemd units (ordering: gowa → shim → agent).
log "installing systemd units"
sudo install -m 0644 "$HERE/gowa.service"        /etc/systemd/system/gowa.service
sudo install -m 0644 "$HERE/wagw-shimmy.service" /etc/systemd/system/wagw-shimmy.service
sudo install -m 0644 "$HERE/agent.service"       /etc/systemd/system/agent.service
sudo systemctl daemon-reload
sudo systemctl enable --now gowa.service wagw-shimmy.service agent.service

log "done. Next: pair WhatsApp (gowa /app/login QR or pairing-code), then run backup.sh for an"
log "immediate post-pair snapshot. Verify: systemctl is-active gowa wagw-shimmy agent"
