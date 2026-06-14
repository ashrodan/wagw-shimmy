# Readiness Remediation Cross-Check

Timestamp: 2026-06-14T19-50-12+1000

Scope: cross-check the readiness review findings against the current codebase and record practical
implementation paths. This is not a replacement for `docs/HARDENING.md`; it is the execution note
for turning the readiness blockers into small, reviewable changes.

## Current verdict

The Rust core is healthy: the documented invariants are covered by unit and in-process e2e tests,
and the local gates passed during review:

- `cargo fmt --all --check`
- `cargo clippy --all-targets -- -D warnings`
- `cargo build`
- `cargo test` after allowing loopback binds for mock e2e servers: 36 unit + 16 e2e
- `bash -n deploy/{fleetctl,provision.sh,render-env.sh,backup.sh,tailscale-up.sh}`

Not ready for live tenant traffic until the P0 reliability and deployment-hardening items below are
closed and the manual acceptance flow in `docs/TESTING.md` is executed.

Operator boundary:

- This project must not create new VMs/boxes.
- VM lifecycle belongs outside this repo: create, allocate, and destroy boxes manually or through a
  separate infra owner/tooling path.
- Repo tooling may read configured hosts, check reachability, inspect service state, provision onto
  an already-declared host, and pair/restart the app stack.
- Provider commands such as `ssh exe.dev new` and `ssh exe.dev rm` should be removed from project
  scripts. If a future operator wants provider lifecycle automation, it should live outside
  `wagw-shimmy` and call this repo only after a host already exists.

## Implementation follow-up review

After the first remediation implementation pass, the main recommendations were largely implemented:
durable queue + bounded worker, `/livez` + `/readyz`, GOWA artifact SHA256 verification, `fleetctl`
existing-host-only provisioning, shim `LoadCredential=`, and the acceptance scaffold.

Remaining code/doc gaps from review:

1. **Strict credential-file errors.** `*_FILE` secret resolution should fail fast when a file path is
   explicitly configured but unreadable, missing, or whitespace-only. The first implementation
   treated read errors as `None`, which is acceptable for "no file configured" but not for
   "operator configured a credential file and it is broken". This matters especially for optional
   `GOWA_BASIC_AUTH_FILE`: a broken file must not silently disable GOWA basic auth.
2. **Forward enqueue/notify ordering.** The durable queue implementation should avoid notifying the
   worker until after the inbound id has been marked in the dedup set, or it should split
   `enqueue()` from `notify()`. Otherwise a fast worker can remove the queued file before a racing
   same-id webhook reaches `enqueue()`, allowing a duplicate forward.
3. **Deploy shell tests in CI.** `docs/TESTING.md` now lists `deploy/tests/*.test.sh` as automated,
   so `.github/workflows/ci.yml` must run them. Manual local passes are useful but not enough for a
   CI gate.

Review verification at that point:

- `cargo fmt --all --check`: pass
- `cargo clippy --all-targets -- -D warnings`: pass
- `cargo test`: pass with loopback permission, 36 unit + 16 e2e
- `bash deploy/tests/fleetctl_boundary.test.sh`: pass
- `bash deploy/tests/checksum_verify.test.sh`: pass
- `bash deploy/tests/render_env_creds.test.sh`: pass

## 1. Agent-forward delivery gap

Evidence:

- `src/server.rs` dedups an inbound id before forwarding to the agent.
- The webhook then acks GOWA immediately and forwards inside a tracked async task.
- `src/agent.rs` returns an error on transport failure or non-2xx, but the task only logs it.
- Because GOWA already received `200 OK`, it will not retry a forward that failed after ack.

Recommended solution:

Add a small durable forward queue before acking an allowed inbound message.

Proposed flow:

1. HMAC verify, parse, structural drop, mention detection, policy evaluation as today.
2. Enqueue the allowed `Inbound` to disk under `/var/lib/wagw/shim/queue/pending/`.
3. Atomically write as `id.tmp`, fsync best-effort, then rename to `id.json`.
4. Mark the id in the in-memory dedup set after enqueue succeeds.
5. Notify the worker only after the dedup mark is complete. A clean implementation can make
   `enqueue()` persist only, then call `queue.notify()` from the handler after `dedup.insert_new`.
6. Ack GOWA.
7. A bounded worker drains pending files and calls `agent.forward`.
8. On success, remove the pending file.
9. On retry exhaustion, move it to `/var/lib/wagw/shim/queue/dead/` and log the path.

Why this shape:

- It preserves ack-fast behavior, so slow LLM turns still do not trigger GOWA duplicate deliveries.
- It survives agent downtime and shim restarts after acceptance.
- It gives operators an audit trail and replay target instead of silent loss.
- It avoids a new runtime dependency; JSON plus atomic file rename is enough for the first tenant.

Rejected alternatives:

- Blocking the webhook on `agent.forward`: breaks the 10 second GOWA timeout constraint and brings
  duplicate replies back under slow agent turns.
- Retrying only inside the spawned task: helps transient agent failures but still loses accepted
  messages on process crash or host restart.
- Moving dedup insertion after successful forward only: allows concurrent duplicate webhook
  deliveries to race into multiple agent forwards while the first delivery is still retrying.

Implementation notes:

- Add `ForwardQueue` as a new module or inside `server.rs` only if it stays small.
- Prefer a `Semaphore`-bounded worker at the same time; this closes the documented P1 unbounded task
  issue while touching the same surface.
- Sanitize filenames from message ids. Use hex or a small local escape helper; do not use raw ids as
  paths.
- Do not wake the worker before the handler records the dedup mark. If notify happens inside
  `enqueue()`, a fast worker can process and delete the file before a racing duplicate webhook sees
  the dedup entry.
- Consider adding `tokio`'s `fs` feature, or use blocking file IO only for the tiny enqueue path.

Tests to add:

- Agent returns 500 twice then 200: webhook still acks quickly, one pending item is eventually
  removed, agent sees one logical message id.
- Agent remains down: message moves to dead-letter after bounded retries.
- Router restart with a pending file: the queue drains it on startup.
- Duplicate webhook while item is pending: one queue item, one eventual forward.
- Duplicate webhook racing with a very fast worker after enqueue: still one eventual forward.

## 2. Deep readiness endpoint

Evidence:

- `/healthz` currently returns static JSON and does not probe GOWA or the agent.
- `fleetctl status` relies on `systemctl is-active` plus log grep, so it can miss dependency drift.

Recommended solution:

Split liveness from readiness.

Proposed endpoints:

- `GET /livez`: process-only liveness, static `{"status":"ok"}`.
- `GET /healthz`: keep as an alias to `/livez` for existing docs/scripts.
- `GET /readyz`: dependency-aware readiness, returning `200` only if required localhost dependencies
  respond within a short timeout.

Readiness probes:

- GOWA: use the same base URL and basic auth as outbound sends. `fleetctl` already depends on GOWA
  `/devices`; verify that endpoint under vendored v8.7.0 and use it unless a cheaper endpoint exists.
- Agent: probe `GET /healthz` only if `spike-rust-agent` exposes it in the deployed version. If not,
  make agent probing optional until the agent contract has a stable readiness endpoint.

Response shape:

```json
{
  "status": "ok",
  "gowa": { "ok": true },
  "agent": { "ok": true }
}
```

Use `503` and the same shape with `ok:false` for failed dependencies. Do not include URLs with
credentials or raw error bodies.

Tests to add:

- `/livez` returns 200 with no mocks.
- `/readyz` returns 200 when mock GOWA and mock agent are healthy.
- `/readyz` returns 503 when GOWA is down.
- `fleetctl status` reads `/readyz` or reports it separately from process liveness.

## 3. GOWA artifact integrity

Evidence:

- `deploy/provision.sh` downloads `GOWA_BINARY_URL` and installs it directly.
- CI builds a pinned GOWA artifact but does not publish or enforce a checksum.

Recommended solution:

Require a checksum next to the artifact.

Implementation path:

1. In `.github/workflows/ci.yml`, after building the GOWA binary, run:
   `sha256sum gowa-${GOWA_TAG}-linux-amd64 > gowa-${GOWA_TAG}-linux-amd64.sha256`
2. Upload both the binary and `.sha256` file as artifacts.
3. Make `deploy/provision.sh` require `GOWA_BINARY_SHA256`.
4. After download, verify:
   `printf '%s  %s\n' "$GOWA_BINARY_SHA256" "$tmp" | sha256sum -c -`
5. Pass `GOWA_BINARY_SHA256` through the host-provisioning command after the target host is already
   declared.

Rejected alternatives:

- Trusting the HTTPS URL alone: protects transport but not artifact mixups, compromised artifact
  storage, or accidental tag drift.
- Rebuilding GOWA on the box: avoids artifact integrity but reintroduces toolchain drift and makes
  provisioning slower and harder to reproduce.

Tests to add:

- `bash -n deploy/provision.sh`.
- A shell-level dry run or small helper test where a known checksum passes and a bad checksum fails.

## 4. Secrets in systemd environments

Evidence:

- The three units use `EnvironmentFile=/etc/*.env`.
- The rendered files are 0600, but secrets still become process environment variables.

Recommended solution:

Move in two phases.

Phase 1:

- Split rendered config into non-secret env files plus secret files under a root-owned credential
  directory.
- Add shim support for `*_FILE` or systemd credential paths for:
  `GOWA_BASIC_AUTH`, `GOWA_WEBHOOK_SECRET`, `WHATSAPP_WEBHOOK_TOKEN`, and
  `WHATSAPP_GATEWAY_TOKEN`.
- Treat an explicitly configured `<NAME>_FILE` as authoritative. If the file cannot be read or
  trims to an empty value, startup must fail with an error naming `<NAME>_FILE`. Only an absent
  `<NAME>_FILE` should fall through to "not configured".
- Add equivalent support in `spike-rust-agent` for its WhatsApp/OpenAI secrets.
- Update `wagw-shimmy.service` and `agent.service` to use `LoadCredential=`.

Phase 2:

- Handle GOWA. If upstream does not support file-based secrets, either carry a minimal vendored patch
  to read credential files or document the residual risk and harden the unit/proc surface. A wrapper
  that reads credentials and exports env vars does not fully solve `/proc` environment exposure.

Tests to add:

- Config loads each secret from direct env and from file path, with file path taking precedence only
  when the direct env is absent.
- Config fails when `<NAME>_FILE` is explicitly set to a missing, unreadable, or whitespace-only
  credential file.
- Error messages still name variables, not values.
- Render script creates secret files with mode 0600.

## 5. `fleetctl` VM lifecycle boundary

Evidence:

- `cmd_add` calls `ssh exe.dev new`, so this repo can create provider boxes.
- `cmd_remove` calls `ssh exe.dev rm`, so this repo also owns provider deletion today.
- `deploy/README.md` documents `fleetctl add` as creating the box.
- The desired boundary is narrower: this repo can check whether a configured host exists and can
  provision the app stack onto it, but it must not create new VMs/boxes.

Recommended solution:

Remove provider VM lifecycle from `fleetctl`.

Required behavior:

1. Replace `fleetctl add <id>` with `fleetctl provision <id>` or make `add` a backwards-compatible
   alias that only provisions an existing host.
2. Require `tenants/<id>.yaml` to contain `box:` or `magicdns:` before any remote action.
3. Validate reachability with non-destructive checks only, for example:
   - `ssh -o BatchMode=yes -o ConnectTimeout=8 <host> true`
   - optional tailnet DNS check for `magicdns`
4. If the host is missing or unreachable, fail with a message telling the operator to create or fix
   the VM outside this repo.
5. Keep the existing rsync, secret resolution, `provision.sh`, `pair`, `status`, and `rotate` flows
   after host validation passes.
6. Change `remove <id>` so it stops services and optionally wipes app-owned data only after explicit
   confirmation, but does not run `exe.dev rm`. The final VM destroy step should be a manual/external
   infra action.
7. Rename user-facing help and comments so they no longer imply this repo creates or destroys boxes.

Docs to update:

- `deploy/README.md`: change lifecycle examples from "create a tenant box" to "register/provision an
  existing tenant box".
- `deploy/fleetctl`: command header and usage text should say `provision`, `pair`, `list`, `status`,
  `rotate`, and `remove-app-data` or similar. Do not mention `exe.dev new` as a supported action.
- `docs/FIRST-NUMBER.md`: if it references VM creation, keep that as a human/operator prerequisite,
  not a repo command.

Rejected alternatives:

- Making VM creation idempotent inside `fleetctl`: safer than today, but still violates the desired
  repo boundary.
- Hiding creation behind a flag such as `--create`: still lets the project create boxes and makes
  accidental provider actions possible from the wrong working tree.

Tests to add:

- `bash -n deploy/fleetctl`.
- A shell test with stubbed `ssh` proving no command path invokes `exe.dev new`.
- A shell test with stubbed `ssh` proving no command path invokes `exe.dev rm`.
- A shell test proving provisioning fails before rsync when no host is declared.
- A shell test proving provisioning runs against an existing configured host after reachability
  succeeds.

## 6. Restore drill

Evidence:

- `backup.sh` snapshots SQLite safely through `.backup`.
- `docs/TESTING.md` still marks restore as manual acceptance, not completed evidence.

Recommended solution:

Before the first real tenant carries traffic:

1. Pair a disposable account.
2. Run `deploy/backup.sh`.
3. Restore into a scratch box.
4. Start GOWA from the restored SQLite store.
5. Confirm the session loads without re-pairing.
6. Record the date, tenant/scratch ids, restic snapshot id, and result in a dated acceptance note.

## 7. CI coverage for deploy shell tests

Evidence:

- `docs/TESTING.md` records the deploy shell tests as automated coverage.
- `.github/workflows/ci.yml` currently runs Rust fmt, clippy, and `cargo test`, but not
  `deploy/tests/*.test.sh`.

Recommended solution:

Add a CI step in the `shim` job:

```yaml
- name: deploy shell tests
  run: |
    bash deploy/tests/checksum_verify.test.sh
    bash deploy/tests/fleetctl_boundary.test.sh
    bash deploy/tests/render_env_creds.test.sh
```

Keep these tests network-free. They should use stubbed `ssh`/`rsync`, temp dirs, and fake secrets
only.

## Suggested order

1. Fix the queue notify/dedup race.
2. Make configured `*_FILE` credential failures strict.
3. Wire `deploy/tests/*.test.sh` into CI.
4. Durable forward queue plus bounded worker.
5. `/livez` and `/readyz`, then wire readiness into `fleetctl status`.
6. GOWA checksum verification in CI and provision.
7. Remove VM lifecycle from `fleetctl`; convert `add` into existing-host provisioning only.
8. Credential-file support for shim and agent, then decide the GOWA path.
9. Restore drill and manual local/tenant acceptance.

This order removes the silent-loss risk first, then improves observability, then closes deployment
supply-chain and operator-flow risks.
