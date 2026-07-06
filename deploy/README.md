# wagw-shimmy fleet deploy

Self-contained provisioning for the exe.dev fleet. **One tenant per box**, full isolation, three
localhost-wired systemd units (`gowa` → `wagw-shimmy` → `agent`). Operator access and fleet peering
go over a single Tailscale tailnet; the data plane never leaves `127.0.0.1`.

> **Operator boundary.** This tooling **never creates or destroys VMs**. Creating the box (and
> setting its `box:`/`magicdns:` in `tenants/<id>.yaml`) and the final VM destroy are external/manual
> infra steps. `fleetctl` only provisions the app stack onto an already-declared, reachable host,
> pairs it, checks its health, and manages app-owned data.

## Files

| file | what |
|------|------|
| `fleet.yaml` | tenant index: id → box host → wa account → status |
| `tenants/<id>.yaml` | per-tenant: box, device jid, policy, secret **names**, backup prefix |
| `provision.sh` | on-box: tailnet join → Rust toolchain → prebuilt GOWA (checksum-verified) → build shim+agent → render env → install+enable units. **Idempotent.** |
| `tailscale-up.sh` | idempotent tailnet join (ephemeral, tagged key; MagicDNS host = tenant id; Tailscale SSH) |
| `tailscale-acl.hujson` | version-controlled tailnet ACL (tag:wagw peers; tag:admin SSH; tag ownership locked) |
| `render-env.sh` | tenant yaml + secret store → `/etc/{gowa,wagw-shimmy,agent}.env` (0600) |
| `gowa.service` / `wagw-shimmy.service` / `agent.service` | hardened systemd units with `After`/`Requires`/`PartOf` ordering |
| `backup.sh` | restic → R2, per-tenant prefix; **SQLite-consistent** snapshot of the whatsmeow store |
| `fleetctl` | operator CLI (run from Mac/phone): `provision` / `pair` / `list` / `status` / `rotate` / `remove` (no VM lifecycle; `add` is a back-compat alias for `provision`) |

To **visualise** this config — per-number policy, channel routing, and derived config gaps — run the
`fleetview` TUI (a workspace member, not part of the shim binary): `cargo run -p fleetview` from the
repo root (`-- --dump` for a non-interactive text dump). See [`../fleetview/README.md`](../fleetview/README.md).

## Per-box stack

```
WhatsApp ⟷ gowa :3000 ──webhook──▶ wagw-shimmy :8080 ──/whatsapp/inbound──▶ agent :3001
                      ◀──/send/message──              ◀────────/send─────────
```

Ports: GOWA `:3000`, shim `:8080`, agent `:3001` — all bound to `127.0.0.1`. Only a read-only
`/healthz` + `fleetctl status` surface is reachable, and only over the tailnet.

## Lifecycle

```sh
# 0. PREREQUISITE (outside this repo): create the VM and set its box:/magicdns: in tenants/acme.yaml.
#    fleetctl never creates a box — provision fails fast if the host is undeclared or unreachable.

# Provision the app stack onto the existing, reachable box, then hand off to interactive pairing.
# GOWA_BINARY_SHA256 is the artifact's published checksum (provision aborts on mismatch).
GOWA_BINARY_URL=https://ci/artifacts/gowa-v8.7.0-linux-amd64 \
GOWA_BINARY_SHA256=<sha256-of-the-artifact> \
  ./fleetctl provision acme
./fleetctl pair acme         # link WhatsApp (pairing-code): captures JID → re-render → backup
#   see ../docs/PAIRING.md for the QR alternative and the full auth flow

./fleetctl list
./fleetctl status            # all tenants: gowa/shim/agent active? /readyz? last inbound? reachable?
./fleetctl status acme

./fleetctl rotate acme whatsapp_gateway_token   # re-render env + restart after a secret change
./fleetctl remove acme       # final backup → stop units → wipe APP DATA (the VM is left intact;
                             # destroy it outside this repo, then mark decommissioned)
```

## Secrets

Names only in `tenants/<id>.yaml`; **values** come from the secret store at provision/render time and
materialise to `/etc/*.env` (0600). Wire `fleetctl`'s `secret_get` to your store (1Password `op` is
the default adapter; env-var fallback `SECRET_<NAME>` otherwise). Includes the **Tailscale auth key**
(ephemeral + tagged + short expiry — never commit). Prefer systemd `LoadCredential=` over plain
`EnvironmentFile=` where feasible. Rotate `WHATSAPP_*` tokens and the restic password via
`fleetctl rotate`.

## Backups (SQLite-consistent + tested restore)

The whatsmeow store is **live SQLite** — `backup.sh` does a WAL checkpoint + `.backup` into a temp
file and restic's *that*, never the hot file (a torn raw copy may not restore). The restic password
lives in the store **and** is escrowed out-of-band — lose it and every backup is unrecoverable.

**Restore drill** (do this, don't assume): restore the latest snapshot into a scratch box and confirm
GOWA loads the session **without re-pairing**. Reboot ≠ re-pair — but only a real restore proves it.

## Networking — Tailscale + Termius

- Boxes join one tailnet (`tailscale-up.sh`), reachable as `wa-<tenant>` via MagicDNS.
- Tailscale SSH (`--ssh`) is ACL-gated — no keypair to copy onto the phone.
- Android: install **Tailscale** (same tailnet) + **Termius**; add each box by its MagicDNS name.
  Termius's host list = the fleet.
- exe.dev's native `<box>.exe.xyz` SSH stays as a break-glass fallback if the tailnet is unreachable.
