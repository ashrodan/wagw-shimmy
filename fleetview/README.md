# fleetview

A terminal admin for the wagw shim fleet. It reads the **live deploy config** —
`deploy/fleet.yaml` + `deploy/tenants/*.yaml` — and shows, per WhatsApp number:

- the **routing pipeline** (WhatsApp → GOWA → `wagw-shimmy` → each downstream channel, incl. the
  implicit `default`), with the agent URL and inbound-token name per channel;
- **who can reach it** — DM/group policy, allowlist counts, `require_mention`, send-rate cap;
- **groups & routing** — each group JID (with the human name scraped from its inline yaml comment)
  → channel → admitted via *channel map* vs *allowlist*;
- **secret names** (never values);
- **health & gaps** — structurally derived: debug-sink dropping inbound, empty DM allowlist,
  redundant allowlist entries, references to an undefined channel, provisioning-without-config, etc.

Read-only. Press `r` to re-read from disk, so it always reflects the current yaml.

## Run

```sh
cargo run -p fleetview                 # auto-locates ./deploy (walks up from cwd)
cargo run -p fleetview -- path/to/deploy
cargo run -p fleetview -- --dump       # non-interactive: flatten every tenant to stdout (pipe/CI)
```

Deploy dir resolution: explicit positional arg → `$FLEETVIEW_DEPLOY_DIR` → nearest `deploy/fleet.yaml`
walking up from the cwd.

Keys: `↑/↓` (or `j/k`) select number · `PgUp/PgDn` (`Home/End`) scroll detail · `r` reload · `q` quit.

## Why a separate crate

`fleetview` is a workspace member, not part of the shim binary, so its `ratatui` / `serde_yaml`
dependencies never enter the service. `cargo build` / `cargo test` at the repo root still act on the
shim alone; build this with `-p fleetview`. It needs the same edition-2024 toolchain as the shim.

Pure parsing/derivation lives in `src/model.rs` (unit-tested, no terminal); `src/main.rs` is only the
TUI. `cargo test -p fleetview` covers the health signals and the comment-label scraper.
