#!/usr/bin/env bash
# tailscale-up.sh — idempotent tailnet join for a fleet box.
#
# Joins the box to the one wagw tailnet with an ephemeral, tagged auth key, sets its MagicDNS
# hostname to the tenant id, and enables Tailscale SSH (identity + ACL-gated, no key copy to manage
# on the operator's phone). Ephemeral means a wiped box auto-drops from the tailnet.
#
# Re-running is safe: if already up with the right hostname, it's a no-op.
#
# Usage: tailscale-up.sh <magicdns-hostname>
#   TS_AUTHKEY must be in the environment (from the secret store; never commit it).
set -euo pipefail

HOSTNAME_ARG="${1:?usage: tailscale-up.sh <magicdns-hostname>}"
: "${TS_AUTHKEY:?TS_AUTHKEY must be set (ephemeral, tagged auth key from the secret store)}"

log() { printf '[tailscale-up] %s\n' "$*"; }

# 1. Install tailscale if missing (idempotent).
if ! command -v tailscale >/dev/null 2>&1; then
  log "installing tailscale"
  curl -fsSL https://tailscale.com/install.sh | sh
fi

# 2. Already joined with the right hostname? Then nothing to do.
if tailscale status --json 2>/dev/null | grep -q '"BackendState": *"Running"'; then
  current="$(tailscale status --json | sed -n 's/.*"HostName": *"\([^"]*\)".*/\1/p' | head -1 || true)"
  if [ "${current:-}" = "$HOSTNAME_ARG" ]; then
    log "already up as ${HOSTNAME_ARG}; no-op"
    exit 0
  fi
fi

# 3. Join: ephemeral + tagged + Tailscale SSH + MagicDNS hostname = tenant id.
log "joining tailnet as ${HOSTNAME_ARG}"
tailscale up \
  --authkey="${TS_AUTHKEY}" \
  --hostname="${HOSTNAME_ARG}" \
  --advertise-tags=tag:wagw \
  --ssh \
  --accept-routes=false \
  --reset

log "tailnet up: $(tailscale ip -4 2>/dev/null | head -1 || echo '?') (${HOSTNAME_ARG})"
