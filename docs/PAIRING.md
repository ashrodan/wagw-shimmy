# WhatsApp pairing (auth flow)

GOWA owns the WhatsApp link (whatsmeow). The shim and agent are never involved in pairing — they
only start carrying traffic once GOWA holds a linked session. Pairing is **interactive and one-time
per tenant**: whatsmeow persists the session in its SQLite store, so a reboot is *not* a re-pair
(only unlinking the device on the phone, or losing the store, forces a re-link).

All endpoints below are verified against the vendored GOWA **v8.7.0** source (`vendor/gowa`). The
whole REST surface — including login — sits behind GOWA's **HTTP Basic Auth** (`GOWA_BASIC_AUTH`).

## TL;DR — the easy path

```sh
fleetctl pair <tenant-id>
```

This runs the pairing-code flow end to end: requests a code from GOWA, prints it for you to enter on
the phone, waits for the link, captures the linked **JID**, writes it into `tenants/<id>.yaml`,
re-renders `/etc/*.env` with `GOWA_DEVICE_ID`, restarts the stack, and takes the immediate post-pair
backup. Works headless (no browser) — ideal from Termius on Android over the tailnet.

## Why a tenant is bound to one box

One tenant per box ⇒ GOWA runs **single-device mode**: the one linked device is the default, so you
don't pass `X-Device-Id` to log in. The device's id equals its JID once linked
(`init.go: instanceID = device.ID.String()`), and that JID is what the shim sends as `X-Device-Id`
on every `/send/message`. The JID is unknowable until *after* pairing — which is why `GOWA_DEVICE_ID`
is a post-pair value and `fleetctl pair` is what fills it in.

## The two GOWA login flows

GOWA binds to `127.0.0.1:3000` (data plane never leaves the box), so both flows reach it over the
tailnet.

### A. Pairing code (headless — what `fleetctl pair` automates)

```sh
# On the box (curl hits localhost with basic auth):
curl -fsS -u "$GOWA_USER:$GOWA_PASS" \
  'http://127.0.0.1:3000/app/login-with-code?phone=61400000001'
# → {"results":{"device_id":"...","pair_code":"ABCD-EFGH"}}
```

On the phone: **WhatsApp → Settings → Linked Devices → Link a device → "Link with phone number
instead"** → enter the `pair_code`.

### B. QR (browser)

```sh
ssh -L 3000:127.0.0.1:3000 wa-<tenant>      # Tailscale SSH port-forward
open http://localhost:3000                  # dashboard (basic auth) → Login → scan the QR
```

Or the raw endpoint `GET /app/login` → `{"results":{"qr_link":"…/statics/qrcode/…png","qr_duration":N}}`
(the QR expires after `qr_duration` seconds; re-hit to refresh). On the phone: **Linked Devices →
Link a device → scan**.

## After the link

1. **Capture the JID.** `GET /devices` lists the device with its now-populated `jid`
   (state `logged_in`). `fleetctl pair` polls this automatically.
2. **Wire it in.** Write `device_jid:` into `tenants/<id>.yaml`, then re-render env so
   `GOWA_DEVICE_ID=<jid>` and restart (`fleetctl pair` / `fleetctl rotate` do this via `render_remote`).
3. **Back up immediately.** The whatsmeow store now holds the session; a crash before the first
   scheduled backup would lose the pairing. `fleetctl pair` runs `backup.sh` for you.

## Re-pairing / recovery

- **Unlinked from the phone, or store lost:** re-run `fleetctl pair <id>` for a fresh code.
- **Restore from backup:** restoring the whatsmeow SQLite store brings the session back **without**
  re-pairing — but only a tested restore proves it (see `deploy/README.md` → restore drill).
- **Logout (deliberate):** `POST /app/logout` (or `/devices/:id/logout`) drops the session; the next
  inbound won't flow until you pair again.

## Endpoint reference (GOWA v8.7.0, all behind basic auth)

| Purpose | Method + path |
|---|---|
| QR login (default device) | `GET /app/login` |
| Pairing-code login | `GET /app/login-with-code?phone=<E164>` |
| List devices (read the linked JID) | `GET /devices` |
| Logout | `POST /app/logout` |
| Web dashboard (QR button) | `GET /` |
