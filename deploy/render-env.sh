#!/usr/bin/env bash
# render-env.sh — render one tenant's on-box config:
#   $ETC_DIR/{gowa,wagw-shimmy,agent}.env   — 0600 env files (non-secret config + GOWA/agent secrets)
#   $CRED_DIR/<secret>                        — 0600 secret files for the shim's systemd LoadCredential=
#
# Runs ON the box (provision.sh calls it). Secret VALUES are resolved from the environment, which
# provision.sh populates from the secret store. This script never embeds a secret literal and never
# echoes a value.
#
# Why credential files: the shim reads its four secrets (GOWA_BASIC_AUTH, GOWA_WEBHOOK_SECRET,
# WHATSAPP_WEBHOOK_TOKEN, WHATSAPP_GATEWAY_TOKEN) from FILES, so they never enter the shim's process
# environment (/proc/<pid>/environ). The systemd unit LoadCredential=s these files and sets each
# <NAME>_FILE=%d/<cred>; the shim still prefers a direct env var, so foreground/manual runs work too.
#
# GOWA has no upstream file-secret support, so /etc/gowa.env keeps APP_BASIC_AUTH +
# WHATSAPP_WEBHOOK_SECRET inline (residual /proc risk — see docs/HARDENING.md; the unit is already
# NoNewPrivileges + ProtectSystem=strict). The agent likewise keeps its secrets inline pending the
# cross-repo *_FILE follow-on filed in spike-rust-agent.
#
# Output dirs are overridable for tests (RENDER_ETC_DIR / RENDER_CRED_DIR); both default under /etc.
#
# Usage: render-env.sh <tenant-id>
#   Required secret env vars (names per tenants/<id>.yaml `secrets:`):
#     GOWA_BASIC_AUTH GOWA_WEBHOOK_SECRET WHATSAPP_WEBHOOK_TOKEN WHATSAPP_GATEWAY_TOKEN
#     OPENAI_BEARER_TOKEN
#   Policy values are passed via env too (provision.sh extracts them from the yaml):
#     WA_DM_POLICY WA_DM_ALLOW WA_GROUP_POLICY WA_GROUP_ALLOW WA_REQUIRE_MENTION
#     WA_FREE_RESPONSE_CHATS WA_SEND_RATE_PER_MIN GOWA_DEVICE_ID
#   Optional per-group channel routing (empty → default-only):
#     WA_CHANNELS  WA_CHANNEL_<L>_URL  WA_CHANNEL_<L>_TOKEN  WA_GROUP_CHANNELS
set -euo pipefail

TENANT="${1:?usage: render-env.sh <tenant-id>}"
ETC_DIR="${RENDER_ETC_DIR:-/etc}"
CRED_DIR="${RENDER_CRED_DIR:-$ETC_DIR/wagw-shimmy.creds}"

require() { for v in "$@"; do [ -n "${!v:-}" ] || { echo "render-env: $v is required" >&2; exit 1; }; done; }
require GOWA_BASIC_AUTH GOWA_WEBHOOK_SECRET WHATSAPP_WEBHOOK_TOKEN WHATSAPP_GATEWAY_TOKEN OPENAI_BEARER_TOKEN GOWA_DEVICE_ID

umask 077   # files created 0600

write() {  # write <path> ; reads heredoc from stdin
  local path="$1"; install -m 0600 /dev/null "$path"; cat > "$path"
}
write_secret() {  # write_secret <name> <value> — a 0600 credential source file (no trailing newline)
  install -m 0600 /dev/null "$CRED_DIR/$1"; printf '%s' "$2" > "$CRED_DIR/$1"
}

mkdir -p "$ETC_DIR"
install -d -m 0700 "$CRED_DIR"   # root-owned credential dir (LoadCredential= reads it as root)

# --- GOWA env (unchanged: upstream has no file-secret support) ---
write "$ETC_DIR/gowa.env" <<EOF
# rendered by render-env.sh for tenant ${TENANT} — do not edit by hand
APP_PORT=3000
APP_BASIC_AUTH=${GOWA_BASIC_AUTH}
WHATSAPP_WEBHOOK=http://127.0.0.1:8080/webhook/gowa
WHATSAPP_WEBHOOK_SECRET=${GOWA_WEBHOOK_SECRET}
WHATSAPP_WEBHOOK_EVENTS=message,message.reaction
WHATSAPP_AUTO_DOWNLOAD_MEDIA=true
EOF

# --- shim secret credentials (0600; loaded via systemd LoadCredential=, never in /proc/environ) ---
write_secret gowa_basic_auth        "${GOWA_BASIC_AUTH}"
write_secret gowa_webhook_secret    "${GOWA_WEBHOOK_SECRET}"
write_secret whatsapp_webhook_token "${WHATSAPP_WEBHOOK_TOKEN}"
write_secret whatsapp_gateway_token "${WHATSAPP_GATEWAY_TOKEN}"

# --- per-group channel routing (optional) ---
# Empty WA_CHANNELS → emit nothing → single "default" channel (current behaviour, e.g. wagw-1).
# For each label L: WA_CHANNEL_<U>_URL is a non-secret base URL (kept in the env file); a non-empty
# WA_CHANNEL_<U>_TOKEN is written to a 0600 credential file and referenced via WA_CHANNEL_<U>_TOKEN_FILE
# (the shim's secret() reads it). NOTE: per-channel tokens are written here but, since channel labels
# are dynamic, the static systemd unit does not yet LoadCredential them — wire a matching
# LoadCredential when standing up distinct per-channel bearers (the multi-target operational step).
# Channels whose agent shares the default inbound bearer need no token at all.
CHANNEL_ENV=""
if [ -n "${WA_CHANNELS:-}" ]; then
  IFS=',' read -r -a _wa_labels <<< "${WA_CHANNELS}"
  for _label in "${_wa_labels[@]}"; do
    _label="$(printf '%s' "$_label" | tr -d '[:space:]')"
    [ -n "$_label" ] || continue
    _u="$(printf '%s' "$_label" | tr 'a-z' 'A-Z')"
    _l="$(printf '%s' "$_label" | tr 'A-Z' 'a-z')"
    _url_var="WA_CHANNEL_${_u}_URL"
    _tok_var="WA_CHANNEL_${_u}_TOKEN"
    _url="${!_url_var:-}"
    [ -n "$_url" ] || { echo "render-env: $_url_var is required for channel '$_label'" >&2; exit 1; }
    CHANNEL_ENV="${CHANNEL_ENV}WA_CHANNEL_${_u}_URL=${_url}"$'\n'
    _tok="${!_tok_var:-}"
    if [ -n "$_tok" ]; then
      write_secret "wa_channel_${_l}_token" "$_tok"
      CHANNEL_ENV="${CHANNEL_ENV}WA_CHANNEL_${_u}_TOKEN_FILE=${CRED_DIR}/wa_channel_${_l}_token"$'\n'
    fi
  done
fi

# --- shim env (NON-SECRET only; the four secrets above come from credential files) ---
write "$ETC_DIR/wagw-shimmy.env" <<EOF
# rendered by render-env.sh for tenant ${TENANT} — do not edit by hand
# Secrets are NOT here — they load from credential files via the systemd unit's LoadCredential=.
SHIM_BIND=127.0.0.1:8080
GOWA_URL=http://127.0.0.1:3000
GOWA_DEVICE_ID=${GOWA_DEVICE_ID}
AGENT_INBOUND_URL=http://127.0.0.1:3001
WA_DM_POLICY=${WA_DM_POLICY:-allowlist}
WA_DM_ALLOW=${WA_DM_ALLOW:-}
WA_GROUP_POLICY=${WA_GROUP_POLICY:-off}
WA_GROUP_ALLOW=${WA_GROUP_ALLOW:-}
WA_REQUIRE_MENTION=${WA_REQUIRE_MENTION:-true}
WA_FREE_RESPONSE_CHATS=${WA_FREE_RESPONSE_CHATS:-}
WA_SEND_RATE_PER_MIN=${WA_SEND_RATE_PER_MIN:-20}
WA_CHANNELS=${WA_CHANNELS:-}
WA_GROUP_CHANNELS=${WA_GROUP_CHANNELS:-}
RUST_LOG=info
EOF

# Append per-channel URL / token-file lines (nothing when WA_CHANNELS is empty).
if [ -n "$CHANNEL_ENV" ]; then
  printf '%s' "$CHANNEL_ENV" >> "$ETC_DIR/wagw-shimmy.env"
fi

# --- agent env (secrets still inline; cross-repo *_FILE follow-on tracked in spike-rust-agent) ---
write "$ETC_DIR/agent.env" <<EOF
# rendered by render-env.sh for tenant ${TENANT} — do not edit by hand
AGENT_BIND_ADDR=127.0.0.1:3001
AGENT_CHANNELS=whatsapp
WHATSAPP_GATEWAY_URL=http://127.0.0.1:8080
WHATSAPP_GATEWAY_TOKEN=${WHATSAPP_GATEWAY_TOKEN}
WHATSAPP_WEBHOOK_TOKEN=${WHATSAPP_WEBHOOK_TOKEN}
OPENAI_BEARER_TOKEN=${OPENAI_BEARER_TOKEN}
EOF

echo "render-env: wrote ${ETC_DIR}/{gowa,wagw-shimmy,agent}.env + ${CRED_DIR}/<secrets> (0600) for ${TENANT}"
