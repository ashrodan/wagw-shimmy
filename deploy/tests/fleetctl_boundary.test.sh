#!/usr/bin/env bash
# fleetctl_boundary.test.sh — proves the operator boundary: fleetctl provisions/removes the app
# stack on an EXISTING box but never creates or destroys a VM.
#
# Strategy: copy fleetctl into an isolated temp dir with its own tenants/ + fleet.yaml, and shim
# `ssh`/`rsync` onto PATH as stubs that log their argv. No network, no real box.
#
# Asserts:
#   1. The fleetctl source invokes no `exe.dev new` / `exe.dev rm`.
#   2. provision against a tenant with NO declared host fails BEFORE any rsync.
#   3. provision against a configured, reachable host runs rsync + provision.sh, and no ssh call
#      ever references `exe.dev new`.
#   4. remove never references `exe.dev rm`.
set -euo pipefail

REPO_DEPLOY="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
FLEETCTL_SRC="$REPO_DEPLOY/fleetctl"
fail() { echo "FAIL: $*" >&2; exit 1; }

# --- 1. static guarantee: no provider lifecycle in the source -------------------------------------
grep -Eq 'exe\.dev[[:space:]]+new' "$FLEETCTL_SRC" && fail "fleetctl source still calls 'exe.dev new'"
grep -Eq 'exe\.dev[[:space:]]+rm'  "$FLEETCTL_SRC" && fail "fleetctl source still calls 'exe.dev rm'"

# --- harness: isolated copy + stubbed ssh/rsync ---------------------------------------------------
work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT
cp "$FLEETCTL_SRC" "$work/fleetctl"
mkdir -p "$work/tenants" "$work/bin" "$work/../spike-rust-agent" 2>/dev/null || mkdir -p "$work/tenants" "$work/bin"

cat > "$work/fleet.yaml" <<'YAML'
tenants:
  - id: withhost
  - id: nohost
YAML

cat > "$work/tenants/withhost.yaml" <<'YAML'
id: withhost
box: wa-withhost.example.invalid
magicdns: wa-withhost
device_jid: "61400000001:1@s.whatsapp.net"
policy:
  dm_policy: allowlist
  group_policy: off
  require_mention: true
  send_rate_per_min: 20
YAML

cat > "$work/tenants/nohost.yaml" <<'YAML'
id: nohost
device_jid: ""
policy:
  dm_policy: off
YAML

SSH_LOG="$work/ssh.log"; RSYNC_LOG="$work/rsync.log"
: > "$SSH_LOG"; : > "$RSYNC_LOG"

# ssh stub: log argv, succeed. The reachability probe is `ssh ... <host> true`, which thus passes.
cat > "$work/bin/ssh" <<EOF
#!/usr/bin/env bash
echo "ssh \$*" >> "$SSH_LOG"
exit 0
EOF
# rsync stub: log argv, succeed.
cat > "$work/bin/rsync" <<EOF
#!/usr/bin/env bash
echo "rsync \$*" >> "$RSYNC_LOG"
exit 0
EOF
chmod +x "$work/bin/ssh" "$work/bin/rsync"

# Secrets via the env-var fallback in secret_get (no `op` needed).
export SECRET_WITHHOST_GOWA_BASIC_AUTH=admin:x
export SECRET_WITHHOST_GOWA_WEBHOOK_SECRET=whsec
export SECRET_WITHHOST_WHATSAPP_WEBHOOK_TOKEN=wtok
export SECRET_WITHHOST_WHATSAPP_GATEWAY_TOKEN=gtok
export SECRET_WITHHOST_OPENAI_BEARER_TOKEN=otok
export SECRET_WITHHOST_TAILSCALE_AUTHKEY=tskey
export GOWA_BINARY_URL="https://example.invalid/gowa"
export GOWA_BINARY_SHA256="0000000000000000000000000000000000000000000000000000000000000000"

run_fleetctl() { PATH="$work/bin:$PATH" bash "$work/fleetctl" "$@"; }

# --- 2. provision with no declared host must fail before rsync ------------------------------------
: > "$RSYNC_LOG"
if run_fleetctl provision nohost >/dev/null 2>&1; then
  fail "provision succeeded for a tenant with no box:/magicdns:"
fi
[ -s "$RSYNC_LOG" ] && fail "provision rsync'd despite no declared host"

# --- 3. provision against a configured, reachable host runs rsync + provision.sh ------------------
: > "$RSYNC_LOG"; : > "$SSH_LOG"
run_fleetctl provision withhost >/dev/null 2>&1 || fail "provision failed against a reachable host"
[ -s "$RSYNC_LOG" ] || fail "provision did not rsync to a reachable host"
grep -q "provision.sh withhost" "$SSH_LOG" || fail "provision did not invoke provision.sh on the host"
grep -q "exe.dev new" "$SSH_LOG" && fail "provision invoked 'exe.dev new'"

# --- 4. remove never invokes exe.dev rm ----------------------------------------------------------
: > "$SSH_LOG"
echo withhost | run_fleetctl remove withhost >/dev/null 2>&1 || fail "remove failed against a reachable host"
grep -q "exe.dev rm" "$SSH_LOG" && fail "remove invoked 'exe.dev rm'"
grep -q "backup.sh withhost" "$SSH_LOG" || fail "remove did not take a final backup"

echo "PASS: fleetctl provisions/removes app stack only — no exe.dev new/rm, host validation enforced"
