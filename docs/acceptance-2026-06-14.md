# Acceptance record — restore drill + live acceptance

> **Status: PENDING EXECUTION.** This is the turnkey record for the readiness remediation's item 6.
> It needs live infra the repo can't stand up on its own — a **paired disposable WhatsApp account**
> and an **operator-created scratch box** (VM lifecycle is an external step; `fleetctl`/this repo
> never create boxes). Run the steps below, fill in every `‹…›`, flip each result, and change this
> banner to **PASSED** (or **FAILED** with notes) and set the real execution date in the filename +
> header. Until then, **a tenant is not cleared for real traffic.**

| field | value |
|-------|-------|
| Execution date | `‹YYYY-MM-DD›` |
| Operator | `‹name›` |
| Live tenant id | `‹tenant›` |
| Scratch box id (operator-created) | `‹scratch-host›` |
| GOWA tag | `v8.7.0` |
| Shim commit | `‹git rev-parse --short HEAD›` |

---

## Part 1 — Restore drill (backup → restore → loads without re-pairing)

Proves backups are *restorable*, not merely *present*: a restored snapshot must reload the whatsmeow
session **without re-pairing** (a reboot is not a re-pair — only a real restore proves it).

### 1. Pair a disposable account
Use `docs/FIRST-NUMBER.md` or `fleetctl pair ‹tenant›`. Confirm the link:
```sh
# on the live box — a non-empty jid means linked
curl -fsS -u "admin:$GOWA_BASIC_PASS" http://127.0.0.1:3000/devices
```
- [ ] Paired. Linked JID: `‹jid›`

### 2. Take a backup
```sh
# on the live box (env from the tenant secret store)
bash deploy/backup.sh ‹tenant›
restic snapshots --tag tenant=‹tenant›        # note the newest snapshot id
```
- [ ] Backup uploaded. restic snapshot id: `‹snapshot-id›`

### 3. Restore into the scratch box
```sh
# on the scratch box (same RESTIC_REPOSITORY + RESTIC_PASSWORD + R2 creds as the tenant)
restic restore ‹snapshot-id› --target /tmp/restore
# the consistent SQLite copy is the .backup taken by backup.sh:
ls -l /tmp/restore/**/whatsapp.db
install -D -o wagw -g wagw -m 0640 /tmp/restore/**/whatsapp.db /var/lib/wagw/gowa/whatsapp.db
```
- [ ] Restored the whatsmeow store onto the scratch box.

### 4. Start GOWA from the restored SQLite
```sh
# on the scratch box — point GOWA at the restored db, basic auth as the tenant
gowa rest --port 3000 --db-uri "file:/var/lib/wagw/gowa/whatsapp.db?_foreign_keys=on"
```
- [ ] GOWA started against the restored store.

### 5. Confirm the session loads WITHOUT re-pairing
```sh
curl -fsS -u "admin:$GOWA_BASIC_PASS" http://127.0.0.1:3000/devices
# → the SAME linked JID as step 1, with NO QR / pairing-code prompt
```
- [ ] Session loaded; JID matches step 1; **no re-pairing was required.**

**Part 1 result:** `‹PASS | FAIL›` — notes: `‹…›`

> Tear down the scratch box afterwards (external/manual step — not a repo command).

---

## Part 2 — Live DM/group acceptance (`docs/TESTING.md` §3)

Gated on the agent's WhatsApp channel being wired + paired (a separate operational step). Run the
full §3 matrix on the provisioned box and record the outcome here.

- [ ] **Health** — `systemctl is-active gowa wagw-shimmy agent` all `active`; `curl 127.0.0.1:8080/livez` → 200; `curl 127.0.0.1:8080/readyz` → 200 (and 503 with GOWA stopped).
- [ ] **DM round-trip** — allowlisted number DMs the account → agent replies in the same DM.
- [ ] **Group round-trip** — plain group msg silent under require-mention; reply-to-bot answered **in the group** (not the sender's DM); `WA_FREE_RESPONSE_CHATS` bypasses mention.
- [ ] **Dedup / timeout** — replay a webhook id twice → agent invoked once; slow agent (>10 s) → no duplicate reply.
- [ ] **Durable forward** — with the agent unreachable, a signed-webhook replay lands in `/var/lib/wagw/shim/queue/dead/`; `systemctl restart wagw-shimmy` with a pending file drains it.
- [ ] **Policy / isolation** — non-allowlisted sender/group dropped at the shim; `is_from_me` ignored; data dir + WA account isolated to this box.

**Part 2 result:** `‹PASS | FAIL›` — notes: `‹…›`

---

## Sign-off

- [ ] Part 1 (restore drill) PASSED.
- [ ] Part 2 (live acceptance) PASSED.
- [ ] Tenant `‹tenant›` cleared for real traffic on `‹date›` by `‹operator›`.
