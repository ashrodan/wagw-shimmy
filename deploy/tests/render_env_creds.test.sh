#!/usr/bin/env bash
# render_env_creds.test.sh — proves render-env.sh writes shim secrets to 0600 credential FILES (for
# systemd LoadCredential=) and keeps them OUT of the rendered shim env file. No real /etc, no secrets
# on disk beyond a throwaway temp dir.
set -euo pipefail

REPO_DEPLOY="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
fail() { echo "FAIL: $*" >&2; exit 1; }

# Portable file-mode read (Linux stat -c vs BSD/macOS stat -f).
file_mode() {
  if stat -c '%a' "$1" >/dev/null 2>&1; then stat -c '%a' "$1"; else stat -f '%Lp' "$1"; fi
}

work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT
etc="$work/etc"; creds="$work/creds"
mkdir -p "$etc"

WEBHOOK_SECRET_VALUE="super-secret-webhook-hmac"

RENDER_ETC_DIR="$etc" RENDER_CRED_DIR="$creds" \
GOWA_BASIC_AUTH="admin:pw" \
GOWA_WEBHOOK_SECRET="$WEBHOOK_SECRET_VALUE" \
WHATSAPP_WEBHOOK_TOKEN="wtok" \
WHATSAPP_GATEWAY_TOKEN="gtok" \
OPENAI_BEARER_TOKEN="otok" \
GOWA_DEVICE_ID="61400000001:1@s.whatsapp.net" \
  bash "$REPO_DEPLOY/render-env.sh" acme >/dev/null

# 1. Each shim secret is a credential file, mode 0600, with the exact value.
for name in gowa_basic_auth gowa_webhook_secret whatsapp_webhook_token whatsapp_gateway_token; do
  [ -f "$creds/$name" ] || fail "credential file $name was not written"
  mode="$(file_mode "$creds/$name")"
  [ "$mode" = "600" ] || fail "credential $name has mode $mode, expected 600"
done
[ "$(cat "$creds/gowa_webhook_secret")" = "$WEBHOOK_SECRET_VALUE" ] \
  || fail "gowa_webhook_secret content does not match the input value"

# 2. The credential dir itself is 0700.
dirmode="$(file_mode "$creds")"
[ "$dirmode" = "700" ] || fail "credential dir has mode $dirmode, expected 700"

# 3. The rendered shim env file must NOT contain the secret values (they load from creds now).
grep -q "$WEBHOOK_SECRET_VALUE" "$etc/wagw-shimmy.env" \
  && fail "shim env file still contains a secret value"
grep -q "GOWA_WEBHOOK_SECRET=" "$etc/wagw-shimmy.env" \
  && fail "shim env file still declares GOWA_WEBHOOK_SECRET inline"

# 4. The env file is itself 0600 and carries the non-secret config.
[ "$(file_mode "$etc/wagw-shimmy.env")" = "600" ] || fail "shim env file is not mode 0600"
grep -q "GOWA_DEVICE_ID=61400000001" "$etc/wagw-shimmy.env" \
  || fail "shim env file missing non-secret GOWA_DEVICE_ID"

echo "PASS: render-env writes 0600 credential files and keeps secrets out of the shim env file"
