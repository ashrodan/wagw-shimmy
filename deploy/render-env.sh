#!/usr/bin/env bash
# render-env.sh — render /etc/{gowa,wagw-shimmy,agent}.env (0600) from a tenant yaml + secret store.
#
# Runs ON the box (provision.sh calls it). Secret VALUES are resolved from the environment, which
# provision.sh populates from the secret store (exe.dev store / 1Password / scp). This script never
# embeds a secret literal and never echoes a value.
#
# Usage: render-env.sh <tenant-id>
#   Required secret env vars (names per tenants/<id>.yaml `secrets:`):
#     GOWA_BASIC_AUTH GOWA_WEBHOOK_SECRET WHATSAPP_WEBHOOK_TOKEN WHATSAPP_GATEWAY_TOKEN
#     OPENAI_BEARER_TOKEN
#   Policy values are passed via env too (provision.sh extracts them from the yaml):
#     WA_DM_POLICY WA_DM_ALLOW WA_GROUP_POLICY WA_GROUP_ALLOW WA_REQUIRE_MENTION
#     WA_FREE_RESPONSE_CHATS WA_SEND_RATE_PER_MIN GOWA_DEVICE_ID
set -euo pipefail

TENANT="${1:?usage: render-env.sh <tenant-id>}"

require() { for v in "$@"; do [ -n "${!v:-}" ] || { echo "render-env: $v is required" >&2; exit 1; }; done; }
require GOWA_BASIC_AUTH GOWA_WEBHOOK_SECRET WHATSAPP_WEBHOOK_TOKEN WHATSAPP_GATEWAY_TOKEN OPENAI_BEARER_TOKEN GOWA_DEVICE_ID

umask 077   # files created 0600

write() {  # write <path> ; reads heredoc from stdin
  local path="$1"; install -m 0600 /dev/null "$path"; cat > "$path"
}

# --- GOWA env (the upstream WhatsApp gateway) ---
write /etc/gowa.env <<EOF
# rendered by render-env.sh for tenant ${TENANT} — do not edit by hand
APP_PORT=3000
APP_BASIC_AUTH=${GOWA_BASIC_AUTH}
WHATSAPP_WEBHOOK=http://127.0.0.1:8080/webhook/gowa
WHATSAPP_WEBHOOK_SECRET=${GOWA_WEBHOOK_SECRET}
WHATSAPP_WEBHOOK_EVENTS=message
WHATSAPP_AUTO_DOWNLOAD_MEDIA=false
EOF

# --- shim env ---
write /etc/wagw-shimmy.env <<EOF
# rendered by render-env.sh for tenant ${TENANT} — do not edit by hand
SHIM_BIND=127.0.0.1:8080
GOWA_URL=http://127.0.0.1:3000
GOWA_BASIC_AUTH=${GOWA_BASIC_AUTH}
GOWA_DEVICE_ID=${GOWA_DEVICE_ID}
GOWA_WEBHOOK_SECRET=${GOWA_WEBHOOK_SECRET}
AGENT_INBOUND_URL=http://127.0.0.1:3001
WHATSAPP_WEBHOOK_TOKEN=${WHATSAPP_WEBHOOK_TOKEN}
WHATSAPP_GATEWAY_TOKEN=${WHATSAPP_GATEWAY_TOKEN}
WA_DM_POLICY=${WA_DM_POLICY:-allowlist}
WA_DM_ALLOW=${WA_DM_ALLOW:-}
WA_GROUP_POLICY=${WA_GROUP_POLICY:-off}
WA_GROUP_ALLOW=${WA_GROUP_ALLOW:-}
WA_REQUIRE_MENTION=${WA_REQUIRE_MENTION:-true}
WA_FREE_RESPONSE_CHATS=${WA_FREE_RESPONSE_CHATS:-}
WA_SEND_RATE_PER_MIN=${WA_SEND_RATE_PER_MIN:-20}
RUST_LOG=info
EOF

# --- agent env ---
write /etc/agent.env <<EOF
# rendered by render-env.sh for tenant ${TENANT} — do not edit by hand
AGENT_BIND_ADDR=127.0.0.1:3001
AGENT_CHANNELS=whatsapp
WHATSAPP_GATEWAY_URL=http://127.0.0.1:8080
WHATSAPP_GATEWAY_TOKEN=${WHATSAPP_GATEWAY_TOKEN}
WHATSAPP_WEBHOOK_TOKEN=${WHATSAPP_WEBHOOK_TOKEN}
OPENAI_BEARER_TOKEN=${OPENAI_BEARER_TOKEN}
EOF

echo "render-env: wrote /etc/{gowa,wagw-shimmy,agent}.env (0600) for ${TENANT}"
