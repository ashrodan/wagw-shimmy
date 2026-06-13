# Part C — Decommission "Bae" on Hermes `bird-tortoise` (permanent)

WhatsApp leaves Hermes **for good**. Telegram **stays** on Hermes (Railway) — no change to
`myhermes/gateway/config.yaml`. This is a *removal*, not a re-provision-in-Hermes. New WhatsApp
tenants are provisioned fresh via `fleetctl add` on new exe.dev boxes.

> ⚠️ Runbook only — these steps touch a live box and the WhatsApp pairing on the phone. Do them
> deliberately, in order. Confirm the wagw-shimmy tenant is already live and answering before
> tearing Bae down, so there's no WhatsApp outage window.

## Pre-check

- [ ] A wagw-shimmy tenant box is `active` and has passed the DM + group round-trip verification.
- [ ] You have console/phone access to remove the linked device.

## Steps (on `ssh bird-tortoise.exe.xyz`)

1. [ ] Stop + disable the Hermes service:
   ```sh
   sudo systemctl stop hermes && sudo systemctl disable hermes
   ```
2. [ ] Remove `WHATSAPP_*` from `~/.hermes/.env`; delete the WhatsApp session + bridge log:
   ```sh
   sed -i '/^WHATSAPP_/d' ~/.hermes/.env
   rm -rf ~/.hermes/whatsapp/session/ ~/.hermes/whatsapp/bridge.log
   ```
   (paths per `myhermes/docs/exe-access.md`)
3. [ ] On the phone: WhatsApp → **Linked Devices** → remove **Bae**.
4. [ ] **Prefer wipe** the box (clean — no residual Baileys state):
   ```sh
   ssh exe.dev rm bird-tortoise
   ```
5. [ ] Record the change:
   - `myhermes/docs/exe-deploy.md` — mark WhatsApp **removed**, point to `wagw-shimmy`.
   - `myhermes/docs/exe-access.md` — note Bae teardown.
   - No change to `myhermes/gateway/config.yaml` (Telegram stays).

## Result

myhermes simplifies: the Baileys/Node 22/wheel-gap WhatsApp vendoring (`docs/exe-deploy.md` gotchas
#3/#5) is no longer needed there. Telegram continues unaffected on Railway. A `tgw-shimmy` sibling
(Telegram off Hermes too) is a possible later follow-on — explicitly out of scope here.
