#!/usr/bin/env bash
# backup.sh — SQLite-consistent restic → R2 backup of a tenant's state, namespaced by prefix.
#
# The whatsmeow session store is LIVE SQLite. A raw file copy mid-write can capture a torn write
# that fails to restore (= losing the WhatsApp pairing). So we snapshot via `sqlite3 .backup` into a
# temp file and restic THAT, never the hot file. Shim + agent state dirs are backed up too.
#
# The restic password lives in the secret store AND is escrowed out-of-band: lose it = lose every
# backup. Run immediately after pairing (so a crash before the first scheduled backup can't lose the
# pairing), then on a schedule (systemd timer / cron).
#
# Usage: backup.sh <tenant-id>
#   Env: RESTIC_PASSWORD, R2_BUCKET, R2_PREFIX, AWS_ACCESS_KEY_ID, AWS_SECRET_ACCESS_KEY,
#        R2_ACCOUNT_ID (for the S3-compatible endpoint).
set -euo pipefail

TENANT="${1:?usage: backup.sh <tenant-id>}"
: "${RESTIC_PASSWORD:?}" "${R2_BUCKET:?}" "${R2_PREFIX:?}" "${R2_ACCOUNT_ID:?}"
: "${AWS_ACCESS_KEY_ID:?}" "${AWS_SECRET_ACCESS_KEY:?}"

export RESTIC_REPOSITORY="s3:https://${R2_ACCOUNT_ID}.r2.cloudflarestorage.com/${R2_BUCKET}/${R2_PREFIX}"
DATA=/var/lib/wagw
DB="$DATA/gowa/whatsapp.db"

log() { printf '[backup:%s] %s\n' "$TENANT" "$*"; }

# Init the repo on first run (idempotent — ignore "already initialized").
restic snapshots >/dev/null 2>&1 || { log "initialising restic repo"; restic init; }

# 1. Consistent SQLite snapshot into a temp file (WAL checkpoint + .backup).
staging="$(mktemp -d)"
trap 'rm -rf "$staging"' EXIT
if [ -f "$DB" ]; then
  log "snapshotting whatsmeow SQLite store"
  sqlite3 "$DB" "PRAGMA wal_checkpoint(TRUNCATE);" >/dev/null 2>&1 || true
  sqlite3 "$DB" ".backup '$staging/whatsapp.db'"
fi

# 2. Back up the SQLite snapshot + shim/agent state dirs under one snapshot, tagged by tenant.
log "uploading restic snapshot"
restic backup \
  --tag "tenant=${TENANT}" \
  --tag "kind=wagw-stack" \
  "$staging" \
  "$DATA/shim" \
  "$DATA/agent"

# 3. Retention: keep recent + tapering history.
restic forget --prune \
  --tag "tenant=${TENANT}" \
  --keep-last 7 --keep-daily 7 --keep-weekly 4 --keep-monthly 6

log "done. Verify a snapshot exists: restic snapshots --tag tenant=${TENANT}"
