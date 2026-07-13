# Fleet orchestration & monitoring (design)

How to run the fleet **from afar** — commission, provision, and decommission tenant boxes on
exe.dev, and watch health across all of them. This is the *operations* companion to
[`deploy/README.md`](../deploy/README.md) (per-box app provisioning) and
[`CHANNEL-CONNECTIONS.md`](CHANNEL-CONNECTIONS.md) (box-to-box wiring).

> **Status: design only.** This document proposes a tool (`fleetadm`) and a monitoring approach.
> No code in this doc — it exists to agree the shape, the privilege model, and the boundary trade
> before anything is built.

## Why this exists

The fleet is a set of **single-tenant-per-box** stacks (GOWA → `wagw-shimmy` → `spike-rust-agent`),
one WhatsApp number per box, on exe.dev, peered over a Tailscale tailnet. That isolation is
deliberate and load-bearing — WhatsApp ToS ban blast-radius, failure isolation, per-account session
state — and is *not* up for revisiting here. The open question is purely **operational**: a static
fleet of these boxes needs a repeatable, auditable way to be stood up, torn down, and watched, run
from the operator's control plane (Mac/phone over the tailnet, or a dedicated control box).

What exists today stops one step short of that, on purpose:

- [`deploy/fleetctl`](../deploy/fleetctl) provisions/pairs/rotates/removes the **app stack** on an
  **already-existing** box. It explicitly **does not** create or destroy VMs.
- That boundary was a conscious decision: the readiness review
  ([`readiness-remediation-2026-06-14T19-50-12+1000.md`](readiness-remediation-2026-06-14T19-50-12+1000.md) §5)
  removed `ssh exe.dev new` / `ssh exe.dev rm` from `fleetctl`, and
  [`deploy/tests/fleetctl_boundary.test.sh`](../deploy/tests/fleetctl_boundary.test.sh) fails CI if
  they ever reappear.
- Health is **pull-only**: `fleetctl status` rolls up `systemctl is-active` + `/readyz` +
  last-inbound over Tailscale SSH. [`HARDENING.md`](HARDENING.md) P1 still lists a Prometheus
  `/metrics` surface and staleness alerting as open.

So "an orchestrator that commissions/provisions/decommissions boxes" is exactly the layer that was
pushed *out* of `fleetctl`. This doc brings a **scoped** version of it back — as a distinct,
privileged tool, not as a change to `fleetctl`.

## 1. The orchestration agent (`fleetadm`)

Define a single **privileged control surface**, working name **`fleetadm`**, distinct from
`fleetctl`. It runs only on the operator control plane and is the **only** component permitted to
touch exe.dev **provider lifecycle** (`exe.dev new` / `exe.dev rm`) and **peering** create/destroy.
Everything app-layer it delegates to the existing `fleetctl` verbs — it composes, it does not
re-implement.

```
            operator control plane (Mac / phone / control box)
            ┌──────────────────────────────────────────────┐
            │  fleetadm   (provider lifecycle + peerings)   │   ← new, privileged
            │     │ composes                                │
            │     ▼                                         │
            │  fleetctl   (provision/pair/status/rotate/    │   ← unchanged, no VM powers
            │              remove — app stack only)         │
            └──────────────────────────────────────────────┘
                       │ Tailscale SSH / exe.dev API
                       ▼
              static fleet of tenant boxes (fleet.yaml)
```

### A conscious exception to the readiness boundary

This **re-opens** provider lifecycle that the readiness review deliberately closed. That is
intentional and scoped — not a silent reversal — and the original intent is preserved by *where* the
powers live:

- The powers stay **out of `fleetctl`**. `fleetctl`'s per-operation safety and the
  `fleetctl_boundary.test.sh` guard remain intact and unchanged. `fleetctl` still never calls
  `exe.dev new`/`rm`.
- The powers are confined to **one clearly-named tool** (`fleetadm`) that an operator runs
  deliberately, with its own credentials and audit trail (see §2). The risk the review was guarding
  against — *accidental* box creation/destruction from the wrong working tree, mixed into routine app
  ops — is mitigated by the separation, not re-introduced.

The boundary moves from "this repo must never touch provider lifecycle" to "provider lifecycle lives
in exactly one privileged, audited place, separate from day-to-day app ops." `fleetctl`'s own
`add`-is-an-alias-for-`provision` framing and its `preflight_host` hard-stop stay exactly as they are.

### "Static fleet" is the constraint that makes this safe

`fleetadm` acts **only on tenants already declared** in
[`deploy/fleet.yaml`](../deploy/fleet.yaml) + [`deploy/tenants/<id>.yaml`](../deploy/tenants). There
is **no dynamic autoscaling and no inventing boxes**:

- **Commission `<id>`** = realize a declared-but-absent tenant (its `fleet.yaml` entry exists,
  `status: provisioning`, but no box yet).
- **Decommission `<id>`** = tear down a declared tenant's box and mark it `decommissioned`.

The YAML stays the single source of truth. `fleetadm` reads intent from it and converges the world to
match; it never originates fleet membership. A box `fleetadm` didn't expect (not in `fleet.yaml`) is
an error to surface, never something it creates or destroys.

## 2. Privilege & safety model

**Credential separation.** The exe.dev **provider token** (the power to create/destroy boxes and
peerings) lives *only* with `fleetadm` on the control plane. It is **never** rendered onto a tenant
box, never written to `/etc/*.env`, never resolved by `provision.sh`. This is a strictly higher
privilege tier than the per-tenant secrets `fleetctl` already resolves via `secret_get` (those are
scoped to one tenant and materialise to `0600` files on that tenant's box). A compromised tenant box
can leak its own tenant secrets; it must not be able to reach the provider token and touch the rest of
the fleet.

**Destructive-action guards.** Provider mutations are gated like `fleetctl`'s existing `cmd_remove`:

- **Typed confirmation** for `commission` and `decommission` (echo the tenant id to proceed), mirroring
  `cmd_remove`'s `ack` pattern.
- **Final backup before any destroy** — reuse [`deploy/backup.sh`](../deploy/backup.sh) (the same
  SQLite-consistent snapshot `fleetctl remove` already takes) *before* a box is destroyed, never after.
- **Append-only audit log** of every provider action (`new`/`rm`, peering create/destroy) with
  timestamp, tenant id, operator, and outcome — the one place that records "a box was created/destroyed
  and by whom." `fleetctl` has no equivalent today because it has no provider powers.

**Idempotence & drift.** Commission must be safe to re-run, like `provision.sh` is. `fleetadm` checks
actual state before each provider mutation — box already exists, peerings already present, already
paired — and **converges** rather than duplicating. A half-finished commission (box created, pairing
not done) re-runs cleanly to completion.

## 3. Commission / decommission flows

`fleetadm` orchestrates the provider steps and delegates every app-layer step to `fleetctl`. The
peering model is exactly as in [`CHANNEL-CONNECTIONS.md`](CHANNEL-CONNECTIONS.md): a shim↔agent link
is **two one-directional peerings**, named `<src>-to-<dst>`.

### Commission `<id>`

```
1. validate     tenant declared in fleet.yaml (status: provisioning) + tenants/<id>.yaml present
2. exe.dev new  create the box                                            ── fleetadm (provider)
3. record       write box:/magicdns: into tenants/<id>.yaml               ── fleetadm
4. peerings     create the two peerings per shim↔agent link:              ── fleetadm (provider)
                  <id>-to-<agent>  (attached to shim,  → agent :8000)     [inbound]
                  <agent>-to-<id>  (attached to agent, → shim  :8080)     [reply]
5. provision    push repo + secrets, build, install units                ── fleetctl provision <id>
6. pair         link WhatsApp (pairing-code), capture JID, backup         ── fleetctl pair <id>
7. gate         all units active + READY                                  ── fleetctl status <id>
8. activate     set status: active in fleet.yaml                          ── fleetadm
```

Steps 2 and 4 are the new provider powers; 5–7 are existing `fleetctl` verbs unchanged. The
`exe.dev new` → record-host ordering matters: `fleetctl provision`'s `preflight_host` hard-stops on an
undeclared/unreachable host, so the box must exist and be recorded first — which is precisely the
seam `fleetadm` fills.

### Decommission `<id>`

```
1. confirm      typed tenant-id confirmation                              ── fleetadm
2. remove       final backup → stop units → wipe /var/lib/wagw           ── fleetctl remove <id>
3. peerings     destroy the tenant's peerings                            ── fleetadm (provider)
4. exe.dev rm   destroy the box                                          ── fleetadm (provider)
5. mark         set status: decommissioned in fleet.yaml                 ── fleetadm
```

`fleetctl remove` already takes the final backup and leaves the VM intact; `fleetadm` then performs the
provider teardown `fleetctl` deliberately won't. This composes cleanly with the existing
[`DECOMMISSION-BAE.md`](DECOMMISSION-BAE.md) runbook (unlink the device on the phone is still a manual
human step).

### Multi-agent topologies (one shim → N agents)

When a shim box fans out to several agent boxes via channel routing
([`CHANNEL-CONNECTIONS.md`](CHANNEL-CONNECTIONS.md) §A), commission adds **N extra peering pairs**
(`<id>-to-<L>` / `<L>-to-<id>` per channel label `L`) and the channel env
(`WA_CHANNELS`, `WA_CHANNEL_<L>_URL`, `WA_GROUP_CHANNELS`) is rendered by the existing
`channels-env.awk` path in `fleetctl`. `fleetadm` owns the peering creation; the env rendering stays
in `fleetctl`/`render-env.sh`. The reply edge stays single (`chat_id`-based routing converges on the
shim's one `/send`), so there is still exactly one `<L>-to-<id>` reply peering per agent box.

## 4. Monitoring — phased

### Phase 1 (now): lightweight pull + alerting

A fleet-wide **`fleetadm status`** rollup built directly on the existing `fleetctl status` mechanics
— per-box `systemctl is-active` for `gowa`/`wagw-shimmy`/`agent`, the shim's `/readyz` (deep GOWA
dependency probe), and last-inbound timestamp, all over Tailscale SSH. This is the same data
`fleetctl status` already gathers; `fleetadm` presents it fleet-wide and feeds alerting.

Add a **cron/monitor on the control plane** (HARDENING P1 "Alerting on staleness") that pages on:

- any unit `inactive`/`failed` or a box `unreachable`,
- `/readyz` not `ready` (process up but GOWA wedged underneath — the drift `is-active` alone misses),
- last-inbound older than a per-tenant threshold.

Tailscale admin "last seen" already covers raw box liveness for free, so the monitor focuses on the
app-level signals SSH-reachability can't see.

This phase needs **no shim code changes** — it's pure orchestration over endpoints that already exist.

### Phase 2 (next): `/metrics` surface

Design (build later) the HARDENING P1 Prometheus `/metrics` surface on the shim's **tailnet
interface**, exposing: inbound count, dedup drops, policy drops by reason, rate-limit hits, forward
failures, GOWA 4xx/5xx, send latency. A central Prometheus/Grafana on the control plane scrapes each
shim over the tailnet; dashboards + alert rules replace the cron heuristics from Phase 1 for the
signals they cover.

**Dependency:** this phase requires shim work that is currently open in HARDENING P1 — the metrics
exporter itself, and ideally `sd_notify` readiness + `WatchdogSec` so process-hang is caught by
systemd rather than only by the external monitor. Phase 1 deliberately does **not** depend on any of
that, which is why it ships first.

**Phase boundary / trigger.** Phase 1 is sufficient while the fleet is small and the question is "is
each box up and serving?" Move to Phase 2 when you need **trend/rate** visibility (drop ratios,
latency distributions, ban-pattern detection) or per-reason policy/rate-limit breakdowns that a
point-in-time pull can't give — i.e. when operating the fleet needs history, not just liveness.

## 5. Open questions & risks

- **Re-opened provider lifecycle.** This brings back the exact capability the readiness review closed.
  The justification is operational repeatability for a static fleet; the mitigations are the separate
  tool, the separate provider credential, typed confirmation, and the audit log (§2). This trade
  should be reviewed explicitly before `fleetadm` is built, and the readiness doc cross-referenced so
  the reversal is on the record.
- **Peering automation is new, sharp surface.** Today peering create/destroy is a **manual** step in
  `CHANNEL-CONNECTIONS.md`. Automating it is the riskiest provider interaction after box destroy
  (a wrong peering silently routes replies to the wrong service — see the `:8080` vs `:3000`
  port-mapping caveat in that doc). It needs its own verification (`curl .../livez` from the attached
  VM) baked into the commission flow before a box is marked `active`.
- **YAML parsing.** `fleetctl` reads `fleet.yaml`/`tenants/*.yaml` with `grep`/`awk`. `fleetadm` does
  more structured reads *and writes* (recording `box:`, flipping `status:`), which may warrant a real
  YAML parser to avoid brittle in-place edits. Decision: keep the `awk` in-place-edit approach
  (consistent with `fleetctl`'s `device_jid` rewrite) vs. adopt a parser for `fleetadm`.
- **Language/shape of `fleetadm`.** Bash (consistent with `fleetctl`, runs anywhere with `ssh`) vs. a
  small Rust binary (testable, typed, shares config types with the shim). Bash is the lower-friction
  default given `fleetctl` is already Bash; revisit if the audit-log/idempotence logic grows.

## Related docs

- [`deploy/README.md`](../deploy/README.md) — per-box stack, `fleetctl` verbs, operator boundary.
- [`CHANNEL-CONNECTIONS.md`](CHANNEL-CONNECTIONS.md) — exe.dev peering model + scale-out.
- [`readiness-remediation-2026-06-14T19-50-12+1000.md`](readiness-remediation-2026-06-14T19-50-12+1000.md) §5 — the boundary this amends.
- [`HARDENING.md`](HARDENING.md) — P1 metrics + alerting items the monitoring phases realize.
- [`PAIRING.md`](PAIRING.md), [`DECOMMISSION-BAE.md`](DECOMMISSION-BAE.md) — the per-tenant lifecycle steps `fleetadm` wraps.
