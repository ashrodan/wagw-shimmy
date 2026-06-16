# WhatsApp pairing (auth flow)

GOWA owns the WhatsApp link (whatsmeow). The shim and agent are never involved in pairing — they
only start carrying traffic once GOWA holds a linked session. Pairing is **interactive and one-time
per tenant**: whatsmeow persists the session in its SQLite store, so a reboot is *not* a re-pair
(only unlinking the device on the phone, or losing the store, forces a re-link).

The whole REST surface — including login — sits behind GOWA's **HTTP Basic Auth** (`GOWA_BASIC_AUTH`).

> **⚠️ This build is MULTI-DEVICE.** Corrected against a live pairing of `wagw-1` on 2026-06-16.
> The earlier "single-device, no `X-Device-Id`, device-id == JID" description was wrong and had never
> been exercised against a real pair. The reality (verified against the running binary):
>
> - Every device-scoped endpoint requires a **device context** — `X-Device-Id: <id>` header (preferred),
>   `?device_id=<id>` query, or `:device_id` path param. Without it you get
>   `400 DEVICE_ID_REQUIRED`. Only the device-CRUD + landing routes (`GET/POST /devices`, `GET /`) are
>   exempt.
> - You must **create a device first** (`POST /devices`). Its id is one *you choose* (we use the
>   tenant id, e.g. `wagw-1`). The id does **not** change to the JID after linking — the JID just
>   populates alongside it. So `GOWA_DEVICE_ID` = **the chosen device id**, not the JID (the shim
>   sends it as `X-Device-Id` on `/send/message`, and GOWA resolves the device by that id).
> - The per-device pairing-code route `POST /devices/:id/login/code` returns
>   `"device login with code is not implemented yet"` in this binary. Use the legacy
>   `GET /app/login-with-code` **with an `X-Device-Id` header** instead (its service path *is*
>   implemented). QR via `GET /app/login` + `X-Device-Id` works normally.
>
> **`fleetctl pair` does not yet work against this build** — it assumes single-device and writes the
> JID into `GOWA_DEVICE_ID`. Until it's fixed (create-device + `X-Device-Id` + id-not-JID), pair the
> first numbers **by hand** using the flow below. Tracked as a follow-up.

## The flow (verified, by hand)

GOWA binds to `127.0.0.1:3000` (data plane never leaves the box); reach it over the tailnet/SSH. Run
on the box so curl hits localhost with basic auth (read it from `/etc/gowa.env` as root — it's 0600):

```sh
# On the box. APP_BASIC_AUTH ("admin:<pass>") comes from /etc/gowa.env.
sudo -n bash -c 'set -a; . /etc/gowa.env; set +a; AUTH="$APP_BASIC_AUTH"

# 1. Create the device (id = your choice; we use the tenant id). Idempotent-ish; errors if it exists.
curl -sS -u "$AUTH" -H "Content-Type: application/json" \
  -X POST "http://127.0.0.1:3000/devices" -d "{\"device_id\":\"wagw-1\"}"

# 2a. Pairing code (headless): note the X-Device-Id header.
curl -sS -u "$AUTH" -H "X-Device-Id: wagw-1" \
  "http://127.0.0.1:3000/app/login-with-code?phone=61400000001"
# → {"results":{"device_id":"wagw-1","pair_code":"8HH2-4PEX"}}

# 2b. …OR QR: returns a short-lived PNG (qr_duration ~30s; re-hit to refresh).
curl -sS -u "$AUTH" -H "X-Device-Id: wagw-1" "http://127.0.0.1:3000/app/login"
# → {"results":{"device_id":"wagw-1","qr_link":".../statics/qrcode/scan-qr-<uuid>.png","qr_duration":30}}
'
```

- **Pairing code** — on the phone: **WhatsApp → Settings → Linked Devices → Link a device → "Link with
  phone number instead"** → enter the `pair_code`. (`phone` is E.164 with no `+`, e.g. AU `0413…` → `61413…`.)
- **QR** — fetch the `qr_link` PNG (behind basic auth) and scan it, or port-forward the dashboard:
  `ssh -L 3000:127.0.0.1:3000 <box>` → open `http://localhost:3000` (basic auth) and scan. The 30s
  PNG window is tight for a hand-fetched image; the browser dashboard auto-refreshes and is easier.

## After the link

1. **Confirm + capture the JID.**
   ```sh
   curl -sS -u "$AUTH" "http://127.0.0.1:3000/devices/wagw-1/status"   # → is_logged_in: true
   curl -sS -u "$AUTH" "http://127.0.0.1:3000/devices"                 # → the device with its jid
   ```
2. **Wire it in.** Set `GOWA_DEVICE_ID=<the chosen device id, e.g. wagw-1>` in `/etc/wagw-shimmy.env`
   (NOT the JID — see the warning above), then restart the shim. Record the JID in `tenants/<id>.yaml`
   `device_jid:` as *informational only*.
3. **Back up immediately.** The whatsmeow store now holds the session; a crash before the first
   scheduled backup would lose the pairing. Run `deploy/backup.sh <id>`.

## Re-pairing / recovery

- **Unlinked from the phone, or store lost:** re-create/re-login the device (flow above) for a fresh code/QR.
- **Restore from backup:** restoring the whatsmeow SQLite store brings the session back **without**
  re-pairing — but only a tested restore proves it (see `deploy/README.md` → restore drill).
- **Logout (deliberate):** `POST /devices/<id>/logout` drops the session; the next inbound won't flow
  until you pair again.

## Endpoint reference (running GOWA build, all behind basic auth)

| Purpose | Method + path | Needs `X-Device-Id`? |
|---|---|---|
| Create a device | `POST /devices` (body `{"device_id":"<id>"}`) | no (this is how you make one) |
| List devices (+ JIDs) | `GET /devices` | no |
| Device status (`is_logged_in`) | `GET /devices/:id/status` | no (id in path) |
| QR login | `GET /app/login` | **yes** |
| Pairing-code login | `GET /app/login-with-code?phone=<E164>` | **yes** |
| Per-device code login | `POST /devices/:id/login/code` | ⚠️ **not implemented** in this binary |
| Logout | `POST /devices/:id/logout` | no (id in path) |
| Web dashboard | `GET /` | no |
